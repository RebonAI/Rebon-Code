// What a handler's answer may be: whatever JavaScript writes as JSON.
//
// A handler returns an ordinary value — an object with an `undefined` field,
// a `Date`, a class instance — and rebon reads JSON. The host writes the one
// as the other the way `JSON.stringify` would. Before it did, such an answer
// threw while the terminal was being built, and the call was never answered:
// rebon waited out its bound and reported a plugin that "did not answer".
import test from 'node:test';
import assert from 'node:assert/strict';
import { PluginHost } from '../src/host.mjs';
import { toWireJson } from '../src/protocol.mjs';

const control = (call_id, method, payload = null) => ({ protocol_version: 2, host_epoch: 7, plugin_id: '$rebon/platform', scope_id: '$rebon/control', scope_generation: 0, call_id, message: { type: 'request', method, payload } });
const scoped = (call_id, method, payload) => ({ protocol_version: 2, host_epoch: 7, plugin_id: 'plugin.a', scope_id: 'scope.a', scope_generation: 1, call_id, message: { type: 'request', method, payload } });
const answered = (sent, call_id) => sent.filter((x) => x.message.type === 'terminal').find((x) => x.call_id === call_id)?.message;

class Point {
  constructor(x, y) { this.x = x; this.y = y; }
  norm() { return Math.hypot(this.x, this.y); }
}

async function hostAnswering(serviceHandlers) {
  const sent = [];
  const writer = { send: async (envelope) => { sent.push(envelope); }, flush: async () => {} };
  const host = new PluginHost(writer, {
    load: async () => ({ services: [...serviceHandlers.keys()], eventTopics: [], serviceHandlers, topicHandlers: new Map() }),
  });
  await host.accept(control('i', 'platform/initialize'));
  await host.accept(control('l', 'plugin/load', { pluginId: 'plugin.a', root: '/pkg', entry: 'index.mjs', adapter: { id: 'native', revision: 1 }, services: [...serviceHandlers.keys()] }));
  await host.accept(scoped('o', 'scope/open', { workspace_root: 'C:/w' }));
  return { host, sent };
}

test('a value is written the way JSON.stringify writes it', () => {
  const when = new Date(Date.UTC(2026, 9, 5));
  const written = toWireJson({
    text: 'hi', context: undefined, run: () => 1, tag: Symbol('t'),
    when, at: new Point(3, 4), list: [1, undefined, () => 2, NaN], inf: Infinity, nested: { gone: undefined, kept: null },
  });
  assert.deepEqual(written, {
    text: 'hi', when: when.toISOString(), at: { x: 3, y: 4 }, list: [1, null, null, null], inf: null, nested: { kept: null },
  });
  assert.equal(JSON.stringify(written), JSON.stringify(JSON.parse(JSON.stringify({
    text: 'hi', context: undefined, when, at: new Point(3, 4), list: [1, undefined, () => 2, NaN], inf: Infinity, nested: { gone: undefined, kept: null },
  }))));
  assert.equal(toWireJson(undefined), undefined);
  assert.throws(() => toWireJson({ big: 1n }), /BigInt/);
  const cyclic = {}; cyclic.self = cyclic;
  assert.throws(() => toWireJson(cyclic), /cyclic/);
});

test('an answer with an undefined field is answered, without that field', async () => {
  const kit = await hostAnswering(new Map([['cmd', async () => ({ text: 'count is 1', context: undefined })]]));
  await kit.host.accept(scoped('s', 'service/call', { service: 'cmd', request: null }));
  const answer = answered(kit.sent, 's');
  assert.equal(answer.status, 'success');
  assert.equal(answer.payload.text, 'count is 1');
  assert.ok(!('context' in answer.payload));
});

test('an answer nothing can write ends the call as an error instead of never', async () => {
  const kit = await hostAnswering(new Map([['big', async () => ({ n: 10n })]]));
  await kit.host.accept(scoped('s', 'service/call', { service: 'big', request: null }));
  const answer = answered(kit.sent, 's');
  assert.equal(answer.status, 'error');
  assert.equal(answer.payload.code, '[INVALID_ANSWER]');
  assert.match(answer.payload.message, /BigInt/);
});

test('a handler that answers nothing answers null', async () => {
  const kit = await hostAnswering(new Map([['void', async () => {}]]));
  await kit.host.accept(scoped('s', 'service/call', { service: 'void', request: null }));
  const answer = answered(kit.sent, 's');
  assert.equal(answer.status, 'success');
  assert.equal(answer.payload, null);
});
