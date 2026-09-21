// What one Cordis entry registered, captured while it mounts.
//
// A Cordis plugin does not know the plugin plane exists: it calls
// `ctx.tools.register(...)`, `ctx.llm.registerAdapter(...)`,
// `ctx.systemPrompt.section(...)`, and the seats it calls are the ones that
// have to turn those into something `plugin/load` can answer with. This is
// where they put it.
//
// The sink reaches a seat through the registering context — `ctx[SINK]`, set
// when the entry's context is derived — for the same reason the old ledger's
// owner stamp did: it is read synchronously at registration time, so an async
// interleaving cannot attribute one plugin's registration to another.
//
// Two rules it enforces, both borrowed from the host's own registrar because a
// composition entry is a plugin and should not get a weaker contract:
//
//   * **Declared first.** A registration the load request did not declare is
//     refused where the plugin made it, rather than arriving later as a ready
//     report rebon has to reject.
//   * **Registration closes.** Once the entry's fiber has resolved, the host
//     has already been told what this plugin provides; a later registration
//     would be a capability nothing downstream knows about.

/** The key an entry's context carries its sink under. */
export const SINK = Symbol.for('rebon.composeSink');
/** The key an entry's context carries its plugin id under. */
export const OWNER = Symbol.for('rebon.composeOwner');

class RegistrationError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
    this.name = 'RegistrationError';
  }
}

export class RegistrationSink {
  #sealed = false;

  constructor(pluginId, declared = {}) {
    this.pluginId = pluginId;
    this.declared = {
      services: new Set(declared.services ?? []),
      eventTopics: new Set(declared.eventTopics ?? []),
      llmProviders: new Set(declared.llmProviders ?? []),
      tools: new Set(declared.tools ?? []),
    };
    /** Protocol-shaped registrations, answered by `plugin/load`. */
    this.services = new Map();
    this.topics = new Map();
    this.llmProviders = new Map();
    this.tools = new Map();
    /** Handles on a session, handed out when a scope opens. */
    this.scopeHandlers = [];
    /** The sessions this plugin is currently attached to.
     *
     *  Every entry gets one of these, not just the ones that ask: it is how a
     *  seat reaches rebon on behalf of code running outside any inbound call.
     *  An agent loop drives its own turn, and the adapter it reaches for a
     *  credential is running on the loop's schedule rather than answering
     *  anything — but it is still that plugin's code, in that session, and the
     *  call it makes should say so. */
    this.sessions = new Set();
    this.scopeHandlers.push((scopeCtx) => {
      this.sessions.add(scopeCtx);
      return () => this.sessions.delete(scopeCtx);
    });
    /** Facts with no protocol counterpart, fetched from the `compose` service. */
    this.sections = [];
    this.catalogs = new Map();
    this.webProviders = [];
  }

  #admit(kind, name, declared) {
    if (this.#sealed) {
      throw new RegistrationError(
        '[REGISTRATION_CLOSED]',
        `${kind} ${JSON.stringify(name)} was registered after ${this.pluginId} finished loading`,
      );
    }
    if (typeof name !== 'string' || name.length === 0) {
      throw new RegistrationError('[WRONG_SHAPE]', `a ${kind} needs a non-empty name`);
    }
    if (!declared.has(name)) {
      throw new RegistrationError(
        '[UNAUTHORIZED_REGISTER]',
        `${kind} ${JSON.stringify(name)} is not declared by ${this.pluginId}'s manifest`,
      );
    }
  }

  service(name, handler) {
    this.#admit('service', name, this.declared.services);
    this.services.set(name, handler);
  }

  topic(name, handler) {
    this.#admit('topic', name, this.declared.eventTopics);
    this.topics.set(name, handler);
  }

  llm(provider, handler) {
    this.#admit('provider', provider, this.declared.llmProviders);
    this.llmProviders.set(provider, handler);
  }

  tool(definition, handler) {
    this.#admit('tool', definition?.name, this.declared.tools);
    this.tools.set(definition.name, { definition, handler });
  }

  /// A handle on the session, for a plugin that acts rather than only answers.
  ///
  /// Not declared, because it is not a capability: every power the handle
  /// carries is one of the declarations above, checked when it is used.
  scope(handler) {
    if (typeof handler !== 'function') {
      throw new RegistrationError('[WRONG_SHAPE]', 'a scope handler must be a function');
    }
    this.scopeHandlers.push(handler);
  }

  // ---- extras ----------------------------------------------------------
  //
  // A model catalog, a prompt section and a web provider are things rebon
  // needs and the protocol has no field for. They are reported by the
  // composition's own `compose` service rather than by widening the ready
  // report, which every plugin shares and only this one would use.

  section(entry) {
    this.sections.push(entry);
  }

  catalog(provider, catalog) {
    this.catalogs.set(provider, catalog);
  }

  webProvider(kind, id, available) {
    this.webProviders.push({ kind, id, available });
  }

  /// Closes registration and hands back what the host asked for.
  seal() {
    this.#sealed = true;
    return this;
  }

  /// The `plugin/load` answer, in the shape the host's own loader produces.
  loaded() {
    return {
      services: [...this.services.keys()],
      eventTopics: [...this.topics.keys()],
      llmProviders: [...this.llmProviders.keys()],
      tools: [...this.tools.values()].map((entry) => entry.definition),
      serviceHandlers: this.services,
      topicHandlers: this.topics,
      llmAdapters: this.llmProviders,
      toolHandlers: new Map([...this.tools].map(([name, entry]) => [name, entry.handler])),
      scopeHandlers: [...this.scopeHandlers],
    };
  }

  /// What the `compose` service reports for this plugin.
  report() {
    return {
      sections: this.sections.map((section) => ({ ...section })),
      providers: [...this.catalogs].map(([provider, catalog]) => ({ provider, ...catalog })),
      webProviders: this.webProviders.map((entry) => ({ ...entry })),
    };
  }
}

/// The session this plugin is attached to, if it is attached to one.
///
/// With more than one open, the first is used: a plugin acting on its own
/// schedule is acting in one session, and which one is a fact about how rebon
/// opened them rather than something guessable here.
export function sessionOf(ctx) {
  const sink = ctx?.[SINK];
  if (sink === undefined) return undefined;
  for (const session of sink.sessions) return session;
  return undefined;
}

/// The sink the code currently registering belongs to, or undefined.
///
/// Undefined is normal: the built-in seats mount into the realm itself, and a
/// seat calling another seat is not a registration by any plugin.
export function sinkOf(ctx) {
  return ctx?.[SINK];
}
