import { randomUUID } from 'node:crypto';
import { createPluginBridge } from './bridge.mjs';
import { CallLedger } from './lifecycle.mjs';
import { loadPlugin } from './loader.mjs';
import { asEntryLoad } from './ownership.mjs';
import { commandInvokeRequest, eventDelivery, eventEmitRequest, eventSubscribeRequest, llmControlRequest, llmStreamRequest, pluginLoadRequest, pluginUnloadRequest, seatCallRequest, serviceCallRequest, toolInvokeRequest } from './methods.mjs';
import { FramingError } from './framing.mjs';
import { CALL_CANCEL_METHOD, chunk as chunkFrame, identityOf, isPlatformControl, ProtocolError, terminal } from './protocol.mjs';

const plain = (value) => value !== null && typeof value === 'object' && !Array.isArray(value)
  && (Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null);
const scopeKey = (value) => JSON.stringify([value.plugin_id, value.scope_id]);

export class PluginHost {
  state = 'pre_initialize'; hostEpoch; ledger; stopped = false; admissionClosed = false;
  // Set while `platform/shutdown` is still being planned, so the read loop can
  // see it without waiting for the dispatch it scheduled to finish.
  shuttingDown = false;
  #scopes = new Map(); #order = [];
  // pluginId -> { phase, services, eventTopics, serviceHandlers, topicHandlers,
  //               llmAdapters, toolHandlers, tools:Set(invokable), inFlight:Set,
  //               scopeHandlers, scopeTeardown:Map(scopeKey -> [fn]) }
  #plugins = new Map();
  // subscriptionId -> { pluginId, scopeId, generation, topic, handler }
  #subscriptions = new Map(); #subscriptionSeq = 0;
  // callId -> the bridge that owns an outbound call, so its terminal comes back
  // to the promise that is waiting for it rather than to a guess.
  #outbound = new Map();
  // callId -> the AbortController a running handler is watching, and the ids a
  // cancel arrived for. Kept apart: a handler may ignore its signal and finish
  // normally, and the terminal has to say which of those happened.
  #running = new Map(); #cancelled = new Set();
  constructor(writer, options = {}) {
    this.writer = writer;
    this.load = options.load ?? loadPlugin;
    // Called once a plugin has finished draining, so whatever the loader built
    // for it can be taken down. The host knows a plugin by its declarations; it
    // has no idea what a loader put behind them, which is exactly why the
    // loader is the one told that the plugin is over.
    this.unload = options.unload ?? (async () => {});
    this.newCallId = options.newCallId ?? randomUUID;
  }

  /// Ends a plugin's life once nothing of it is still running.
  ///
  /// Awaited by every caller, and always before the terminal that lets rebon
  /// believe the drain is done: a loader's teardown that ran after rebon had
  /// already loaded a replacement would be disposing the new one's world.
  async #retire(pluginId, plugin) {
    plugin.phase = 'unloaded';
    for (const key of [...plugin.scopeTeardown.keys()]) await this.#closeScopeHandles(pluginId, key);
    await this.unload(pluginId);
  }

  /// Routes one frame by what it is. Requests are work; terminals answer work
  /// this host asked for; a notification is neither and owes nothing back.
  async accept(envelope) {
    switch (envelope.message.type) {
      case 'request': return this.dispatch(envelope);
      case 'terminal': return this.settleUpstream(envelope);
      case 'notification': return this.#acceptNotification(envelope);
      default: throw new ProtocolError('unexpected_message', 'message type is not routable');
    }
  }

  /// Cancel is the only notification the protocol defines, and it asks rather
  /// than commands: the handler may ignore its signal and finish anyway. What
  /// this does is record the intent and raise the signal; the terminal still
  /// comes from whatever the handler does next.
  #acceptNotification(envelope) {
    if (envelope.message.method !== CALL_CANCEL_METHOD) return undefined;
    this.ledger.cancel(envelope);
    const controller = this.#running.get(envelope.call_id);
    if (!controller) return undefined;
    this.#cancelled.add(envelope.call_id);
    controller.abort();
    return undefined;
  }

  /// Hands a terminal back to the call that is waiting for it.
  settleUpstream(envelope) {
    const owner = this.#outbound.get(envelope.call_id);
    if (!owner) throw new ProtocolError('unknown_call', 'terminal does not answer any call this host made');
    this.#outbound.delete(envelope.call_id);
    owner.bridge.receive(envelope);
  }

  /// An upstream caller bound to one scope incarnation.
  ///
  /// A new one per incarnation on purpose: the identity a call carries is the
  /// scope's generation at the time it was made, and reusing a bridge across an
  /// advance would send calls stamped with an incarnation that has ended.
  #bridgeFor(pluginId, scopeId, generation) {
    const holder = {};
    holder.bridge = createPluginBridge({
      trustedPluginId: pluginId,
      scopeBinding: { hostEpoch: this.hostEpoch, scopeId, scopeGeneration: generation },
      transport: this.writer,
      ledger: this.ledger,
      idFactory: () => { const id = this.newCallId(); this.#outbound.set(id, holder); return id; },
    });
    return holder.bridge;
  }

  /// The one bridge belonging to a scope's current incarnation.
  ///
  /// Cached on the scope record, which is replaced whenever the generation
  /// advances — so a new incarnation gets a new bridge without anyone having to
  /// remember to invalidate the old one.
  #bridgeOf(value) {
    const scope = this.#scopes.get(scopeKey(value));
    scope.bridge ??= this.#bridgeFor(value.plugin_id, value.scope_id, value.scope_generation);
    return scope.bridge;
  }

  /// What a plugin is given for the lifetime of one scope incarnation.
  ///
  /// The same powers a call context carries, minus the two that only make sense
  /// inside a call: there is no `emit`, because there is no call to put a chunk
  /// on, and no `signal`, because nothing is being cancelled. What is left is
  /// the session — which is what a plugin that acts on its own schedule needs,
  /// and the reason this exists at all.
  ///
  /// It stops working when its incarnation does. A handle captured across a
  /// generation advance would be speaking for a session that has ended, and
  /// saying so here names that, rather than letting rebon refuse a frame whose
  /// origin is no longer obvious.
  #scopeContextFor(value) {
    const scope = this.#scopes.get(scopeKey(value));
    const plugin = this.#plugins.get(value.plugin_id);
    const gate = { open: true };
    const guard = (what) => {
      if (!gate.open) {
        throw new ProtocolError('[SCOPE_CLOSED]', `${what} belongs to a scope incarnation that has ended`);
      }
    };
    const ctx = Object.freeze({
      scopeId: value.scope_id,
      workspaceRoot: scope?.workspaceRoot,
      publish: async (topic, event = null, options = {}) => {
        guard(`publishing ${JSON.stringify(topic)}`);
        if (!plugin.publishedTopics.has(topic)) {
          throw new ProtocolError('[UNAUTHORIZED_TOPIC]', `topic ${JSON.stringify(topic)} is not declared by the manifest for ${JSON.stringify(value.plugin_id)}`);
        }
        return this.#upstream(value, 'event/emit', eventEmitRequest({ topic, event }), options, `topic ${topic}`);
      },
      seat: async (seat, method, params = null, options = {}) => {
        guard(`seat ${JSON.stringify(seat)}`);
        if (!plugin.seats.has(seat)) {
          throw new ProtocolError('[UNAUTHORIZED_SEAT]', `seat ${JSON.stringify(seat)} is not declared by the manifest for ${JSON.stringify(value.plugin_id)}`);
        }
        return this.#upstream(value, 'seat/call', seatCallRequest({ seat, method, params }), options, `seat ${seat}`);
      },
      invoke: async (tool, input = null, options = {}) => {
        guard(`tool ${JSON.stringify(tool)}`);
        if (!plugin.tools.has(tool)) {
          throw new ProtocolError('[UNAUTHORIZED_TOOL]', `tool ${JSON.stringify(tool)} is not declared by the manifest for ${JSON.stringify(value.plugin_id)}`);
        }
        return this.#upstream(value, 'tool/invoke', toolInvokeRequest({ tool, input }), options, `tool ${tool}`);
      },
    });
    return { ctx, close: () => { gate.open = false; } };
  }

  /// Runs the plugin's scope handlers for one incarnation, keeping whatever
  /// they hand back so closing the scope can undo it.
  async #openScopeHandles(value) {
    const plugin = this.#plugins.get(value.plugin_id);
    if (!plugin || plugin.phase !== 'ready' || plugin.scopeHandlers.length === 0) return;
    const key = scopeKey(value);
    const teardown = [];
    for (const handler of plugin.scopeHandlers) {
      const { ctx, close } = this.#scopeContextFor(value);
      // A handler that throws fails the open: a plugin that believes it is
      // attached to a session and is not would be worse than a refused scope.
      const disposer = await handler(ctx);
      teardown.push(async () => {
        close();
        if (typeof disposer === 'function') await disposer();
      });
    }
    plugin.scopeTeardown.set(key, teardown);
  }

  /// Undoes them. Failures are diagnostics: a scope is closing either way, and
  /// one plugin's bad teardown must not strand the rest.
  async #closeScopeHandles(pluginId, key) {
    const plugin = this.#plugins.get(pluginId);
    const teardown = plugin?.scopeTeardown.get(key);
    if (teardown === undefined) return;
    plugin.scopeTeardown.delete(key);
    for (const undo of teardown.reverse()) {
      try {
        await undo();
      } catch { /* a scope closes whatever its plugins think about it */ }
    }
  }

  /// What a handler is given alongside its request.
  ///
  /// This is the only thing a plugin ever holds that reaches back into rebon,
  /// and it is bound to the call's own scope incarnation: a handler cannot act
  /// on a session other than the one it was called for, because it has no way
  /// to name one.
  #contextFor(value) {
    const scope = this.#scopes.get(scopeKey(value));
    const plugin = this.#plugins.get(value.plugin_id);
    // `open` closes when the handler returns. A ctx captured and used later
    // would be emitting into a call that has already ended, and the refusal has
    // to name that rather than letting the frame reach a caller who stopped
    // reading.
    const gate = { open: true };
    const controller = new AbortController();
    this.#running.set(value.call_id, controller);
    const ctx = Object.freeze({
      scopeId: value.scope_id,
      workspaceRoot: scope?.workspaceRoot,
      // Raised when rebon asks this call to stop. A handler that watches it can
      // stop early; one that ignores it finishes normally, and the terminal
      // says so.
      signal: controller.signal,
      emit: async (piece) => {
        if (!gate.open) throw new ProtocolError('[STREAM_CLOSED]', 'the call this context belongs to has already ended');
        const frame = chunkFrame(identityOf(value), piece);
        this.ledger.chunk(frame, 'inbound');
        await this.writer.send(frame);
      },
      // Publishes one event onto rebon's event plane.
      //
      // Not `emit`: that puts a piece on *this* call, for the caller who is
      // reading it. This puts a fact on the plane, for whoever is listening —
      // a different audience and a different lifetime. Declared as
      // `publishedTopics`, separately from the topics the plugin listens to,
      // because publishing and listening are different powers.
      publish: async (topic, event = null, options = {}) => {
        if (!plugin.publishedTopics.has(topic)) {
          throw new ProtocolError('[UNAUTHORIZED_TOPIC]', `topic ${JSON.stringify(topic)} is not declared by the manifest for ${JSON.stringify(value.plugin_id)}`);
        }
        return this.#upstream(value, 'event/emit', eventEmitRequest({ topic, event }), options, `topic ${topic}`);
      },
      // Kernel seats the plugin was installed to use. A request, so it has an
      // answer — and therefore cannot serve a cordis disposer, which is
      // synchronous. Teardown relies on unload draining, not on calls made on
      // the way out.
      seat: async (seat, method, params = null, options = {}) => {
        if (!plugin.seats.has(seat)) {
          throw new ProtocolError('[UNAUTHORIZED_SEAT]', `seat ${JSON.stringify(seat)} is not declared by the manifest for ${JSON.stringify(value.plugin_id)}`);
        }
        return this.#upstream(value, 'seat/call', seatCallRequest({ seat, method, params }), options, `seat ${seat}`);
      },
      invoke: async (tool, input = null, options = {}) => {
        // Refused here as well as by rebon, for the same reason registration is
        // checked on both sides: this refusal points at the plugin's own line,
        // rather than arriving as a rejection from across a process boundary.
        if (!plugin.tools.has(tool)) {
          throw new ProtocolError('[UNAUTHORIZED_TOOL]', `tool ${JSON.stringify(tool)} is not declared by the manifest for ${JSON.stringify(value.plugin_id)}`);
        }
        return this.#upstream(value, 'tool/invoke', toolInvokeRequest({ tool, input }), options, `tool ${tool}`);
      },
    });
    return { ctx, close: () => { gate.open = false; this.#running.delete(value.call_id); } };
  }

  /// One upstream call, with rebon's own refusal code passed through.
  ///
  /// The bridge reports *that* a call failed; rebon's code and message are in
  /// the payload. A plugin author needs the latter — "the call failed" sends
  /// them nowhere.
  async #upstream(value, method, payload, options, what) {
    try {
      return await this.#bridgeOf(value).call(method, payload, options);
    } catch (cause) {
      const refusal = cause?.payload;
      if (!refusal?.code) throw cause;
      throw new ProtocolError(refusal.code, refusal.message ?? `${what} was refused`);
    }
  }

  #dropSubscriptions(matches) {
    const dropped = [];
    for (const [id, record] of this.#subscriptions) {
      if (!matches(record)) continue;
      this.#subscriptions.delete(id);
      dropped.push(id);
    }
    return dropped;
  }

  async dispatch(envelope) {
    if (this.admissionClosed) throw new ProtocolError('admission_closed', 'host no longer accepts requests');
    if (envelope.message.type !== 'request') throw new ProtocolError('unexpected_message', 'host accepts lifecycle requests only');

    let plan;
    try { plan = this.#plan(envelope); }
    catch (error) {
      if (!(error instanceof ProtocolError)) throw error;
      plan = { error };
    }

    const ledger = this.ledger ?? new CallLedger(envelope.host_epoch);
    ledger.register(identityOf(envelope), 'inbound', 'plugin_host');
    this.ledger ??= ledger;

    if (plan.error) return this.#sendTerminal(envelope, 'error', {
      code: plan.error.code,
      message: plan.error.message.slice(0, 256),
    });
    let payload;
    try { payload = await plan.run(); }
    catch (error) {
      // Only the transport failing is fatal. A plugin's own exception is *that
      // call* failing — letting it out of here would take the host and every
      // other plugin down with one bad handler, which is the opposite of the
      // isolation loading them separately is for.
      if (error instanceof FramingError) throw error;
      // A handler that stopped because it was asked to did not fail. Reporting
      // it as an error would make a deliberate stop look like a fault.
      const status = this.#cancelled.delete(envelope.call_id) ? 'cancelled' : 'error';
      return this.#sendTerminal(envelope, status, {
        code: error?.code ?? '[HANDLER_FAILED]',
        message: String(error?.message ?? 'the handler failed').slice(0, 256),
      });
    }
    this.#cancelled.delete(envelope.call_id);
    await this.#sendTerminal(envelope, 'success', payload);
    if (plan.finalize) await plan.finalize();
  }
  #plan(value) {
    switch (value.message.method) {
      case 'platform/initialize': return this.#planInitialize(value);
      case 'scope/open': return this.#planOpen(value);
      case 'scope/close': return this.#planClose(value);
      case 'platform/shutdown': return this.#planShutdown(value);
      case 'plugin/load': return this.#planLoad(value);
      case 'plugin/unload': return this.#planUnload(value);
      case 'service/call': return this.#planServiceCall(value);
      case 'event/deliver': return this.#planEventDeliver(value);
      case 'llm/stream': return this.#planLlmStream(value);
      case 'llm/control': return this.#planLlmControl(value);
      case 'command/invoke': return this.#planCommandInvoke(value);
      case 'tool/call': return this.#planToolCall(value);
      default: return { error: new ProtocolError('unknown_method', 'method is not supported by this host slice') };
    }
  }
  #planInitialize(value) {
    if (this.state !== 'pre_initialize') throw new ProtocolError('already_initialized', 'initialize is accepted exactly once');
    if (!isPlatformControl(value)) throw new ProtocolError('control_identity_required', 'initialize requires reserved control identity');
    const payload = value.message.payload;
    if (payload !== null) {
      if (!plain(payload)) throw new ProtocolError('initialize_payload', 'initialize payload must be null or an object');
      if (Object.hasOwn(payload, 'workspace') || Object.hasOwn(payload, 'workspace_root')) throw new ProtocolError('workspace_in_initialize', 'workspace belongs only in scope/open');
      if (Object.hasOwn(payload, 'host_epoch') && payload.host_epoch !== value.host_epoch) throw new ProtocolError('epoch_mismatch', 'payload host_epoch must equal envelope host_epoch');
    }
    return { run: async () => {
      this.hostEpoch = value.host_epoch;
      this.state = 'initialized';
      return { capabilities: { scope_lifecycle: true } };
    } };
  }
  #planOpen(value) {
    this.#requireInitialized();
    if (isPlatformControl(value)) throw new ProtocolError('scope_identity_required', 'scope/open cannot use control identity');
    const payload = value.message.payload;
    if (!plain(payload) || Object.keys(payload).length !== 1 || typeof payload.workspace_root !== 'string')
      throw new ProtocolError('scope_open_payload', 'scope/open payload is exactly {workspace_root:string}');
    const key = scopeKey(value); const current = this.#scopes.get(key);
    if (current && value.scope_generation < current.generation) throw new ProtocolError('scope_generation_regression', 'scope generation regressed');
    if (current && value.scope_generation > current.generation && current.open) throw new ProtocolError('future_scope_generation', 'open cannot advance an already-open scope');
    return { run: async () => {
      this.ledger.advanceScope(value.plugin_id, value.scope_id, value.scope_generation, true);
      if (!current) this.#order.push(key);
      this.#scopes.set(key, { pluginId: value.plugin_id, scopeId: value.scope_id, generation: value.scope_generation, open: true, workspaceRoot: payload.workspace_root });
      const subscribed = await this.#subscribeTopics(value);
      await this.#openScopeHandles(value);
      return { opened: true, subscriptions: subscribed };
    } };
  }
  #planClose(value) {
    this.#requireInitialized();
    if (value.message.payload !== null) throw new ProtocolError('scope_close_payload', 'scope/close payload must be null');
    const key = scopeKey(value); const current = this.#scopes.get(key);
    if (!current) throw new ProtocolError('unknown_scope', 'scope is not registered');
    if (value.scope_generation < current.generation) throw new ProtocolError('scope_generation_regression', 'scope generation regressed');
    if (value.scope_generation === current.generation && current.open) throw new ProtocolError('scope_close_not_advanced', 'scope close must carry an advanced generation');
    return { run: async () => {
      this.ledger.closeScope(value.plugin_id, value.scope_id, value.scope_generation);
      // State first, teardown after. Handlers may await, and a window between
      // the ledger knowing the scope has advanced and this table knowing it is
      // a window in which a concurrent shutdown reads a generation the ledger
      // has already left behind.
      this.#scopes.set(key, { ...current, generation: value.scope_generation, open: false });
      await this.#closeScopeHandles(value.plugin_id, key);
      // The generation advance is the invalidation. Nothing bound to the old
      // incarnation survives it, so the records go with it rather than waiting
      // to be discovered as stale on a later delivery.
      const revoked = this.#dropSubscriptions((record) => record.pluginId === value.plugin_id && record.scopeId === value.scope_id);
      return { closed: true, revokedSubscriptions: revoked };
    } };
  }
  #planShutdown(value) {
    this.#requireInitialized();
    if (!isPlatformControl(value)) throw new ProtocolError('control_identity_required', 'shutdown requires reserved control identity');
    if (value.message.payload !== null) throw new ProtocolError('shutdown_payload', 'shutdown payload must be null');
    // Set while planning rather than while running: the read loop has to be
    // able to tell that a frame arrived after shutdown without first awaiting
    // the dispatch it just scheduled.
    this.shuttingDown = true;
    return {
      run: async () => {
        this.admissionClosed = true;
        this.state = 'shutting_down';
        for (const key of [...this.#order].reverse()) {
          const scope = this.#scopes.get(key);
          if (scope?.open) {
            this.ledger.advanceScope(scope.pluginId, scope.scopeId, scope.generation, false);
            this.#scopes.set(key, { ...scope, open: false });
            await this.#closeScopeHandles(scope.pluginId, key);
          }
        }
        return { shutdown: true };
      },
      finalize: async () => {
        await this.writer.flush();
        this.state = 'stopped'; this.stopped = true;
      },
    };
  }
  // Loading and unloading are addressed to the reserved control identity: the
  // plugin being named has no scope of its own until it is loaded, and after an
  // unload it has none again.
  #planLoad(value) {
    this.#requireInitialized();
    if (!isPlatformControl(value)) throw new ProtocolError('control_identity_required', 'plugin/load requires reserved control identity');
    const request = pluginLoadRequest(value.message.payload);
    const current = this.#plugins.get(request.pluginId);
    if (current && current.phase !== 'unloaded') throw new ProtocolError('[PLUGIN_ALREADY_LOADED]', `plugin ${request.pluginId} is already loaded`);
    // Wrapped so an async task this plugin starts can be traced back to it,
    // and so a rejection that arrives while the load is still in flight is
    // known to be this load's rather than someone else's later work.
    return { run: () => asEntryLoad(request.pluginId, async () => {
      const loaded = await this.load(request);
      this.#plugins.set(request.pluginId, {
        phase: 'ready',
        services: loaded.services,
        eventTopics: loaded.eventTopics,
        serviceHandlers: loaded.serviceHandlers,
        topicHandlers: loaded.topicHandlers,
        llmAdapters: loaded.llmAdapters ?? new Map(),
        toolHandlers: loaded.toolHandlers ?? new Map(),
        commandHandlers: loaded.commandHandlers ?? new Map(),
        // The consuming direction: what this plugin may call, from the
        // manifest alone.
        tools: new Set(request.invokableTools),
        seats: new Set(request.seats),
        publishedTopics: new Set(request.publishedTopics),
        scopeHandlers: loaded.scopeHandlers ?? [],
        scopeTeardown: new Map(),
        inFlight: new Set(),
      });
      return {
        pluginId: request.pluginId,
        services: [...loaded.services],
        eventTopics: [...loaded.eventTopics],
        llmProviders: [...(loaded.llmProviders ?? [])],
        llmAdapters: { ...(loaded.llmAdapterInfo ?? {}) },
        tools: [...(loaded.tools ?? [])],
        commands: [...(loaded.commands ?? [])],
      };
    }) };
  }

  // Draining, not deleting: routing stops immediately and whatever was already
  // running is reported rather than forgotten.
  #planUnload(value) {
    this.#requireInitialized();
    if (!isPlatformControl(value)) throw new ProtocolError('control_identity_required', 'plugin/unload requires reserved control identity');
    const request = pluginUnloadRequest(value.message.payload);
    const current = this.#plugins.get(request.pluginId);
    if (!current) throw new ProtocolError('[UNKNOWN_PLUGIN]', `plugin ${request.pluginId} is not loaded`);
    if (current.phase !== 'ready') throw new ProtocolError('[STALE_PROVIDER]', `plugin ${request.pluginId} is ${current.phase}`);
    return { run: async () => {
      current.phase = 'draining';
      const revokedSubscriptions = this.#dropSubscriptions((record) => record.pluginId === request.pluginId);
      const outstandingCalls = [...current.inFlight];
      if (outstandingCalls.length === 0) await this.#retire(request.pluginId, current);
      return { pluginId: request.pluginId, outstandingCalls, revokedSubscriptions };
    } };
  }

  #planServiceCall(value) {
    this.#requireInitialized();
    if (isPlatformControl(value)) throw new ProtocolError('scope_identity_required', 'service/call cannot use control identity');
    const request = serviceCallRequest(value.message.payload);
    const current = this.#plugins.get(value.plugin_id);
    if (!current) throw new ProtocolError('[UNKNOWN_PLUGIN]', `plugin ${value.plugin_id} is not loaded`);
    if (current.phase !== 'ready') throw new ProtocolError('[STALE_PROVIDER]', `plugin ${value.plugin_id} is ${current.phase}`);
    const handler = current.serviceHandlers.get(request.service);
    if (!handler) throw new ProtocolError('[UNKNOWN_SERVICE]', `plugin ${value.plugin_id} does not provide service ${request.service}`);
    return { run: async () => {
      current.inFlight.add(value.call_id);
      const { ctx, close } = this.#contextFor(value);
      try {
        return await handler(request.request, ctx);
      } finally {
        close();
        current.inFlight.delete(value.call_id);
        // A drain that was waiting on this call finishes here, which is the
        // only place it can: nothing else knows the call ended.
        if (current.phase === 'draining' && current.inFlight.size === 0) await this.#retire(value.plugin_id, current);
      }
    } };
  }

  // A plugin that registered a topic handler has said it wants that topic's
  // events; a subscription is that wish bound to one scope incarnation, which
  // is why it is made here and not at load time — at load time there is no
  // scope to pin it to.
  //
  // Awaited on purpose: `scope/open` succeeding while its subscriptions silently
  // did not would leave a plugin that believes it is listening and is not.
  async #subscribeTopics(value) {
    const plugin = this.#plugins.get(value.plugin_id);
    if (!plugin || plugin.phase !== 'ready' || plugin.topicHandlers.size === 0) return [];
    const bridge = this.#bridgeOf(value);
    const registered = [];
    for (const [topic, handler] of plugin.topicHandlers) {
      const subscription = `sub-${++this.#subscriptionSeq}`;
      try {
        await bridge.call('event/subscribe', eventSubscribeRequest({ subscription, topic }));
      } catch (cause) {
        // The bridge reports *that* a call was refused; rebon's own code and
        // message are in the payload. Passing them through is the difference
        // between "scope/open failed" and "that topic is not declared".
        const refusal = cause?.payload;
        throw new ProtocolError(refusal?.code ?? cause?.code ?? '[SUBSCRIBE_FAILED]',
          `subscribing ${topic} was refused: ${refusal?.message ?? cause?.message ?? 'no reason given'}`);
      }
      this.#subscriptions.set(subscription, {
        pluginId: value.plugin_id, scopeId: value.scope_id, generation: value.scope_generation, topic, handler,
      });
      registered.push(subscription);
    }
    return registered;
  }

  // Delivery is refused before the handler runs whenever the subscription no
  // longer describes what is being delivered. A generation mismatch is the one
  // that matters most: running the handler would hand a plugin an event from a
  // session it has already finished.
  #planEventDeliver(value) {
    this.#requireInitialized();
    if (isPlatformControl(value)) throw new ProtocolError('scope_identity_required', 'event/deliver cannot use control identity');
    const delivery = eventDelivery(value.message.payload);
    const record = this.#subscriptions.get(delivery.subscription);
    if (!record || record.pluginId !== value.plugin_id) throw new ProtocolError('[UNKNOWN_SUBSCRIPTION]', `subscription ${delivery.subscription} is not registered for ${value.plugin_id}`);
    if (record.topic !== delivery.topic) throw new ProtocolError('[TOPIC_MISMATCH]', `subscription ${delivery.subscription} is on ${record.topic}, not ${delivery.topic}`);
    if (record.scopeId !== value.scope_id || record.generation !== value.scope_generation)
      throw new ProtocolError('[STALE_SUBSCRIPTION]', `subscription ${delivery.subscription} belongs to another scope incarnation`);
    const plugin = this.#plugins.get(value.plugin_id);
    if (!plugin) throw new ProtocolError('[UNKNOWN_PLUGIN]', `plugin ${value.plugin_id} is not loaded`);
    if (plugin.phase !== 'ready') throw new ProtocolError('[STALE_PROVIDER]', `plugin ${value.plugin_id} is ${plugin.phase}`);
    return { run: async () => {
      // A delivery in progress is work a drain has to wait for, exactly like a
      // service call: a plugin torn down mid-handler is the same hazard.
      plugin.inFlight.add(value.call_id);
      const { ctx, close } = this.#contextFor(value);
      try {
        await record.handler(delivery.event, ctx);
        return { delivered: true };
      } finally {
        close();
        plugin.inFlight.delete(value.call_id);
        if (plugin.phase === 'draining' && plugin.inFlight.size === 0) await this.#retire(value.plugin_id, plugin);
      }
    } };
  }

  // A model turn: routed by provider, answered with chunks and then one
  // terminal. The turn's contents are opaque here — what a chunk means belongs
  // to the model contract, and this host only carries it.
  #planLlmStream(value) {
    this.#requireInitialized();
    if (isPlatformControl(value)) throw new ProtocolError('scope_identity_required', 'llm/stream cannot use control identity');
    const request = llmStreamRequest(value.message.payload);
    const current = this.#plugins.get(value.plugin_id);
    if (!current) throw new ProtocolError('[UNKNOWN_PLUGIN]', `plugin ${value.plugin_id} is not loaded`);
    if (current.phase !== 'ready') throw new ProtocolError('[STALE_PROVIDER]', `plugin ${value.plugin_id} is ${current.phase}`);
    const adapter = current.llmAdapters.get(request.provider);
    if (!adapter) throw new ProtocolError('[UNKNOWN_PROVIDER]', `plugin ${value.plugin_id} has no adapter for ${request.provider}`);
    return { run: async () => {
      current.inFlight.add(value.call_id);
      const { ctx, close } = this.#contextFor(value);
      try {
        return await adapter(request.request, ctx);
      } finally {
        close();
        current.inFlight.delete(value.call_id);
        if (current.phase === 'draining' && current.inFlight.size === 0) await this.#retire(value.plugin_id, current);
      }
    } };
  }

  // Something about the conversation around an adapter's turns, rather than
  // about one turn. An adapter that keeps no state has nothing to do with any
  // of the three signals, so a missing handler is not an error — the answer is
  // the same either way, and refusing would make every stateless provider
  // implement a no-op to stay loadable.
  #planLlmControl(value) {
    this.#requireInitialized();
    if (isPlatformControl(value)) throw new ProtocolError('scope_identity_required', 'llm/control cannot use control identity');
    const request = llmControlRequest(value.message.payload);
    const current = this.#plugins.get(value.plugin_id);
    if (!current) throw new ProtocolError('[UNKNOWN_PLUGIN]', `plugin ${value.plugin_id} is not loaded`);
    if (current.phase !== 'ready') throw new ProtocolError('[STALE_PROVIDER]', `plugin ${value.plugin_id} is ${current.phase}`);
    const adapter = current.llmAdapters.get(request.provider);
    if (!adapter) throw new ProtocolError('[UNKNOWN_PROVIDER]', `plugin ${value.plugin_id} has no adapter for ${request.provider}`);
    return { run: async () => {
      current.inFlight.add(value.call_id);
      const { ctx, close } = this.#contextFor(value);
      try {
        return typeof adapter.control === 'function'
          ? await adapter.control(request.signal, ctx) ?? null
          : null;
      } finally {
        close();
        current.inFlight.delete(value.call_id);
        if (current.phase === 'draining' && current.inFlight.size === 0) await this.#retire(value.plugin_id, current);
      }
    } };
  }

  // A slash command someone typed. Like a tool call in direction and unlike it
  // in audience: the answer is text for the person who typed it, so there is
  // no schema to validate against and no permission to have asked for — the
  // person asking *is* the permission.
  //
  // Only a `prompt` command has a handler here. An `explain` and a `panel`
  // answered at registration, so rebon never sends one.
  #planCommandInvoke(value) {
    this.#requireInitialized();
    if (isPlatformControl(value)) throw new ProtocolError('scope_identity_required', 'command/invoke cannot use control identity');
    const request = commandInvokeRequest(value.message.payload);
    const current = this.#plugins.get(value.plugin_id);
    if (!current) throw new ProtocolError('[UNKNOWN_PLUGIN]', `plugin ${value.plugin_id} is not loaded`);
    if (current.phase !== 'ready') throw new ProtocolError('[STALE_PROVIDER]', `plugin ${value.plugin_id} is ${current.phase}`);
    const handler = current.commandHandlers.get(request.name);
    if (!handler) throw new ProtocolError('[UNKNOWN_COMMAND]', `plugin ${value.plugin_id} does not provide command ${request.name}`);
    return { run: async () => {
      current.inFlight.add(value.call_id);
      const { ctx, close } = this.#contextFor(value);
      try {
        return await handler(request, ctx) ?? null;
      } finally {
        close();
        current.inFlight.delete(value.call_id);
        if (current.phase === 'draining' && current.inFlight.size === 0) await this.#retire(value.plugin_id, current);
      }
    } };
  }

  // The mirror of a service call: rebon is the caller and the plugin owns the
  // tool. Whether the user permitted this run was settled before it got here.
  #planToolCall(value) {
    this.#requireInitialized();
    if (isPlatformControl(value)) throw new ProtocolError('scope_identity_required', 'tool/call cannot use control identity');
    const request = toolInvokeRequest(value.message.payload);
    const current = this.#plugins.get(value.plugin_id);
    if (!current) throw new ProtocolError('[UNKNOWN_PLUGIN]', `plugin ${value.plugin_id} is not loaded`);
    if (current.phase !== 'ready') throw new ProtocolError('[STALE_PROVIDER]', `plugin ${value.plugin_id} is ${current.phase}`);
    const handler = current.toolHandlers.get(request.tool);
    if (!handler) throw new ProtocolError('[UNKNOWN_TOOL]', `plugin ${value.plugin_id} does not provide tool ${request.tool}`);
    return { run: async () => {
      current.inFlight.add(value.call_id);
      const { ctx, close } = this.#contextFor(value);
      try {
        return await handler(request.input, ctx);
      } finally {
        close();
        current.inFlight.delete(value.call_id);
        if (current.phase === 'draining' && current.inFlight.size === 0) await this.#retire(value.plugin_id, current);
      }
    } };
  }

  #requireInitialized() { if (this.state !== 'initialized') throw new ProtocolError('not_initialized', 'host is not initialized'); }
  async #sendTerminal(value, status, payload) {
    const reply = terminal(identityOf(value), status, payload);
    this.ledger.terminal(reply, 'inbound');
    await this.writer.send(reply);
  }
}
