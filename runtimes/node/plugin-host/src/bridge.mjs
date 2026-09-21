import { randomUUID } from 'node:crypto';
import { cancel, notification, request, ProtocolError } from './protocol.mjs';

export function createPluginBridge({ trustedPluginId, scopeBinding, transport, ledger, idFactory = randomUUID }) {
  if (typeof trustedPluginId !== 'string') throw new TypeError('trustedPluginId must be a string');
  if (typeof idFactory !== 'function') throw new TypeError('idFactory must be a function');
  const pending = new Map();
  const makeIdentity = () => Object.freeze({ host_epoch: scopeBinding.hostEpoch, plugin_id: trustedPluginId,
    scope_id: scopeBinding.scopeId, scope_generation: scopeBinding.scopeGeneration, call_id: idFactory() });
  const call = (method, payload, { signal } = {}) => {
    if (signal !== undefined && !(signal instanceof AbortSignal)) throw new TypeError('signal must be an AbortSignal');
    const identity = makeIdentity();
    ledger.register(identity, 'outbound', trustedPluginId);
    let resolvePromise, rejectPromise;
    const promise = new Promise((resolve, reject) => { resolvePromise = resolve; rejectPromise = reject; });
    let settled = false; let cancelSent = false; let requestSent = false; let abortWanted = signal?.aborted ?? false; let listenerAdded = false;
    const cleanup = () => {
      if (listenerAdded) { signal.removeEventListener('abort', onAbort); listenerAdded = false; }
      pending.delete(identity.call_id);
    };
    const settle = (action, value) => {
      if (settled) return;
      settled = true; cleanup(); action(value);
    };
    const sendCancel = () => {
      if (settled || cancelSent || !requestSent) return Promise.resolve();
      cancelSent = true;
      return Promise.resolve().then(() => transport.send(cancel(identity))).catch((error) => settle(rejectPromise, error));
    };
    const onAbort = () => {
      if (settled) return;
      abortWanted = true;
      if (requestSent) void sendCancel();
    };
    pending.set(identity.call_id, { identity, resolve: (value) => settle(resolvePromise, value), reject: (error) => settle(rejectPromise, error) });
    if (signal && !signal.aborted) {
      signal.addEventListener('abort', onAbort, { once: true });
      listenerAdded = true;
    }
    Promise.resolve().then(() => transport.send(request(identity, method, payload))).then(() => {
      if (settled) return;
      requestSent = true;
      if (abortWanted) return sendCancel();
    }).catch((error) => {
      if (!settled) ledger.retire(identity);
      settle(rejectPromise, error);
    });
    return promise;
  };
  const notify = (method, payload) => Promise.resolve().then(() => transport.send(notification(makeIdentity(), method, payload)));
  const receive = (envelope) => {
    const disposition = ledger.terminal(envelope, 'outbound');
    if (disposition.stale) return;
    const item = pending.get(envelope.call_id);
    if (!item) throw new ProtocolError('unknown_call', 'no pending bridge call');
    if (envelope.message.status === 'success') item.resolve(envelope.message.payload);
    else {
      const error = new ProtocolError(envelope.message.status, `call ended with ${envelope.message.status}`);
      error.payload = envelope.message.payload;
      item.reject(error);
    }
  };
  return Object.freeze({ call, notify, receive, pendingCount: () => pending.size });
}
