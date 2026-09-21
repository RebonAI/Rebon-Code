// Streaming answers, from the host's side.
//
// A handler emits pieces through the context it was called with, and then
// returns. The return value is the end; the pieces are chunks on the same call.
// The context closes when the handler returns, because a chunk after the end
// would reach a caller who has already finished reading.
import test from 'node:test';
import assert from 'node:assert/strict';
import { PluginHost } from '../src/host.mjs';

const control = (call_id, method, payload = null) => ({ protocol_version: 1, host_epoch: 7, plugin_id: '$rebon/platform', scope_id: '$rebon/control', scope_generation: 0, call_id, message: { type: 'request', method, payload } });
const scoped = (call_id, method, generation, payload) => ({ protocol_version: 1, host_epoch: 7, plugin_id: 'plugin.a', scope_id: 'scope.a', scope_generation: generation, call_id, message: { type: 'request', method, payload } });

function harness() {
  const sent = [];
  const escaped = {};
  const writer = { send: async (envelope) => { sent.push(envelope); }, flush: async () => {} };
  const serviceHandlers = new Map([
    ['count', async (request, ctx) => {
      for (let index = 0; index < request.n; index++) await ctx.emit({ index });
      return { total: request.n };
    }],
    ['leak', async (_request, ctx) => { escaped.ctx = ctx; return { escaped: true }; }],
    ['late', async () => {
      try {
        await escaped.ctx.emit({ index: 99 });
        return { refused: null };
      } catch (cause) {
        return { refused: cause.code };
      }
    }],
    // Waits to be stopped, then reports how it was stopped.
    ['patient', async (request, ctx) => {
      await new Promise((resolve, reject) => {
        ctx.signal.addEventListener('abort', () => (request.throwOnStop ? reject(new Error('stopped')) : resolve()), { once: true });
      });
      return { stopped: true };
    }],
  ]);
  const host = new PluginHost(writer, {
    load: async () => ({ services: [...serviceHandlers.keys()], eventTopics: [], serviceHandlers, topicHandlers: new Map() }),
  });
  return { host, sent };
}

const answered = (sent, call_id) => sent.filter((x) => x.message.type === 'terminal').find((x) => x.call_id === call_id)?.message;
const chunks = (sent, call_id) => sent.filter((x) => x.message.type === 'chunk' && x.call_id === call_id).map((x) => x.message.payload.index);

async function ready(kit) {
  await kit.host.accept(control('i', 'platform/initialize'));
  await kit.host.accept(control('l', 'plugin/load', { pluginId: 'plugin.a', root: '/pkg', entry: 'index.mjs', services: ['count', 'leak', 'late', 'patient'] }));
  await kit.host.accept(scoped('o', 'scope/open', 1, { workspace_root: 'C:/w' }));
  return kit;
}

test('a handler emits chunks on its own call and then ends it', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'count', request: { n: 3 } }));

  assert.deepEqual(chunks(kit.sent, 's'), [0, 1, 2]);
  // The chunks all precede the terminal, which is what "then it ended" means on
  // a stream that is read in order.
  const order = kit.sent.filter((x) => x.call_id === 's').map((x) => x.message.type);
  assert.deepEqual(order, ['chunk', 'chunk', 'chunk', 'terminal']);
  assert.equal(answered(kit.sent, 's').payload.total, 3);
});

test('a handler that emits nothing is an ordinary single answer', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'count', request: { n: 0 } }));
  assert.deepEqual(chunks(kit.sent, 's'), []);
  assert.equal(answered(kit.sent, 's').status, 'success');
});

// The context belongs to one call. Holding on to it past the end and emitting
// again would put a frame on a call the caller has stopped reading.
test('a context that outlived its call cannot emit', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('a', 'service/call', 1, { service: 'leak', request: null }));
  await kit.host.accept(scoped('b', 'service/call', 1, { service: 'late', request: null }));
  assert.equal(answered(kit.sent, 'b').payload.refused, '[STREAM_CLOSED]');
  assert.equal(chunks(kit.sent, 'a').length, 0, 'nothing was emitted after the end');
});

const cancelFor = (call_id) => ({ protocol_version: 1, host_epoch: 7, plugin_id: 'plugin.a', scope_id: 'scope.a', scope_generation: 1, call_id, message: { type: 'notification', method: 'call/cancel', payload: null } });

// Cancel asks; it does not command. A handler that stops by returning has not
// failed, and reporting it as an error would make a deliberate stop look like a
// fault.
test('a handler that stops by returning ends as a success', async () => {
  const kit = await ready(harness());
  const running = kit.host.accept(scoped('s', 'service/call', 1, { service: 'patient', request: { throwOnStop: false } }));
  await kit.host.accept(cancelFor('s'));
  await running;
  assert.equal(answered(kit.sent, 's').status, 'success');
  assert.equal(answered(kit.sent, 's').payload.stopped, true);
});

test('a handler that stops by throwing ends as cancelled, not as an error', async () => {
  const kit = await ready(harness());
  const running = kit.host.accept(scoped('s', 'service/call', 1, { service: 'patient', request: { throwOnStop: true } }));
  await kit.host.accept(cancelFor('s'));
  await running;
  assert.equal(answered(kit.sent, 's').status, 'cancelled');
});

// A handler that never saw a cancel and threw is a genuine failure.
test('a throw with no cancel behind it is still an error', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'missing', request: null }));
  assert.equal(answered(kit.sent, 's').status, 'error');
  assert.equal(answered(kit.sent, 's').payload.code, '[UNKNOWN_SERVICE]');
});

test('a cancel for a call that is not running is ignored', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'count', request: { n: 1 } }));
  // The ledger knows the call; nothing is running under it any more.
  await kit.host.accept(cancelFor('s'));
  assert.equal(answered(kit.sent, 's').status, 'success');
});
