import { ProtocolError, identityOf } from './protocol.mjs';

const scopeKey = (pluginId, scopeId) => JSON.stringify([pluginId, scopeId]);
const key = (identity) => scopeKey(identity.plugin_id, identity.scope_id);
const same = (a, b) => ['host_epoch','plugin_id','scope_id','scope_generation','call_id'].every((field) => a[field] === b[field]);

export class CallLedger {
  #scopes = new Map(); #calls = new Map();
  constructor(hostEpoch) {
    if (!Number.isSafeInteger(hostEpoch) || hostEpoch < 0) throw new ProtocolError('unsafe_integer', 'host_epoch is invalid');
    this.hostEpoch = Object.is(hostEpoch, -0) ? 0 : hostEpoch;
  }
  scopeGeneration(pluginId, scopeId) { return this.#scopes.get(scopeKey(pluginId, scopeId)); }
  advanceScope(pluginId, scopeId, generation, open = true) {
    if (!Number.isSafeInteger(generation) || generation < 0) throw new ProtocolError('unsafe_integer', 'scope_generation is invalid');
    const key = scopeKey(pluginId, scopeId);
    const current = this.#scopes.get(key);
    if (current && generation < current.generation) throw new ProtocolError('scope_generation_regression', 'scope generation regressed');
    if (current && generation === current.generation) {
      if (open && current.open) return 'idempotent';
      if (!open && !current.open) return 'idempotent';
      current.open = open;
      return 'updated';
    }
    this.#scopes.set(key, { generation, open });
    return current ? 'advanced' : 'established';
  }
  closeScope(pluginId, scopeId, generation) {
    const current = this.#scopes.get(scopeKey(pluginId, scopeId));
    if (!current) throw new ProtocolError('unknown_scope', 'scope is not registered');
    if (generation < current.generation) throw new ProtocolError('scope_generation_regression', 'scope generation regressed');
    if (generation === current.generation && current.open) throw new ProtocolError('scope_close_not_advanced', 'scope close must carry an advanced generation');
    return this.advanceScope(pluginId, scopeId, generation, false);
  }
  register(identity, direction, owner) {
    if (direction !== 'inbound' && direction !== 'outbound') throw new ProtocolError('bad_direction', 'call direction is invalid');
    if (direction === 'inbound') this.#classifyEpoch(identity, true);
    else this.#classify(identity, true);
    if (this.#calls.has(identity.call_id)) throw new ProtocolError('call_id_reused', 'call_id is globally unique within an epoch');
    this.#calls.set(identity.call_id, Object.freeze({ identity: identityOf(identity), direction, owner, terminal: null, registered: true }));
  }
  retire(identity) {
    const current = this.#calls.get(identity.call_id);
    if (!current || !current.registered) throw new ProtocolError('unknown_call', 'call is not registered');
    if (!same(current.identity, identity)) throw new ProtocolError('identity_mismatch', 'full call identity does not match');
    if (current.terminal) throw new ProtocolError('already_terminal', 'terminal call cannot be aborted');
    this.#calls.set(identity.call_id, Object.freeze({ ...current, registered: false }));
  }
  terminal(envelope, direction) {
    if (direction !== 'inbound' && direction !== 'outbound') throw new ProtocolError('bad_direction', 'terminal direction is invalid');
    const epochStale = this.#classifyEpoch(envelope, false); if (epochStale) return { stale: epochStale };
    const current = this.#calls.get(envelope.call_id);
    if (current && (current.identity.plugin_id !== envelope.plugin_id || current.identity.scope_id !== envelope.scope_id))
      throw new ProtocolError('identity_mismatch', 'plugin_id or scope_id does not match');
    const scopeStale = direction === 'outbound' ? this.#classifyScope(envelope, false) : null; if (scopeStale) return { stale: scopeStale };
    if (!current || !current.registered) throw new ProtocolError('unknown_call', 'call is not registered');
    if (current.direction !== direction) throw new ProtocolError('direction_mismatch', `call belongs to ${current.direction} direction`);
    if (!same(current.identity, envelope)) throw new ProtocolError('identity_mismatch', 'full call identity does not match');
    if (envelope.message.type !== 'terminal') throw new ProtocolError('not_terminal', 'message is not terminal');
    if (current.terminal) throw new ProtocolError('duplicate_terminal', `terminal already committed as ${current.terminal}`);
    this.#calls.set(envelope.call_id, Object.freeze({ ...current, terminal: envelope.message.status }));
    return { committed: envelope.message.status, owner: current.owner };
  }
  /// Validates one chunk of a call's answer. Chunks are checked, not recorded:
  /// a stream of a million of them must not grow the ledger.
  ///
  /// The boundary is what the ledger owns — **a chunk after the terminal is an
  /// error**, not a late arrival to drop. A duplicate terminal is a race two
  /// peers can legitimately lose; a chunk after the end means the producer kept
  /// emitting after saying it was done.
  chunk(envelope, direction) {
    if (direction !== 'inbound' && direction !== 'outbound') throw new ProtocolError('bad_direction', 'chunk direction is invalid');
    const epochStale = this.#classifyEpoch(envelope, false); if (epochStale) return { stale: epochStale };
    const current = this.#calls.get(envelope.call_id);
    if (current && (current.identity.plugin_id !== envelope.plugin_id || current.identity.scope_id !== envelope.scope_id))
      throw new ProtocolError('identity_mismatch', 'plugin_id or scope_id does not match');
    const scopeStale = this.#classifyScope(envelope, false); if (scopeStale) return { stale: scopeStale };
    if (!current || !current.registered) throw new ProtocolError('unknown_call', 'call is not registered');
    if (current.direction !== direction) throw new ProtocolError('direction_mismatch', `call belongs to ${current.direction} direction`);
    if (!same(current.identity, envelope)) throw new ProtocolError('identity_mismatch', 'full call identity does not match');
    if (envelope.message.type !== 'chunk') throw new ProtocolError('not_chunk', 'message is not a chunk');
    if (current.terminal) throw new ProtocolError('chunk_after_terminal', `call already ended as ${current.terminal}`);
    return { accepted: true };
  }
  cancel(envelope) {
    const epochStale = this.#classifyEpoch(envelope, false); if (epochStale) return { stale: epochStale };
    const current = this.#calls.get(envelope.call_id);
    if (current && (current.identity.plugin_id !== envelope.plugin_id || current.identity.scope_id !== envelope.scope_id))
      throw new ProtocolError('identity_mismatch', 'plugin_id or scope_id does not match');
    const scopeStale = this.#classifyScope(envelope, false); if (scopeStale) return { stale: scopeStale };
    if (!current || !current.registered) throw new ProtocolError('unknown_call', 'call is not registered');
    if (!same(current.identity, envelope)) throw new ProtocolError('identity_mismatch', 'full call identity does not match');
    if (envelope.message.type !== 'notification' || envelope.message.method !== 'call/cancel' || envelope.message.payload !== null)
      throw new ProtocolError('not_cancel', 'message is not an exact cancel intent');
    return { accepted: true };
  }
  #classify(identity, registration) {
    const epochStale = this.#classifyEpoch(identity, registration); if (epochStale) return epochStale;
    return this.#classifyScope(identity, registration);
  }
  #classifyEpoch(identity, registration) {
    if (identity.host_epoch < this.hostEpoch) return registration ? (() => { throw new ProtocolError('stale_identity', 'old host epoch'); })() : 'host_epoch';
    if (identity.host_epoch > this.hostEpoch) throw new ProtocolError('future_host_epoch', 'future host epoch');
    return null;
  }
  #classifyScope(identity, registration) {
    const scope = this.#scopes.get(key(identity));
    if (!scope) throw new ProtocolError('unknown_scope', 'scope is not registered');
    if (identity.scope_generation < scope.generation) return registration ? (() => { throw new ProtocolError('stale_identity', 'old scope generation'); })() : 'scope_generation';
    if (identity.scope_generation > scope.generation) throw new ProtocolError('future_scope_generation', 'future scope generation');
    if (registration && !scope.open) throw new ProtocolError('scope_closed', 'scope is closed to admission');
    return null;
  }
}
