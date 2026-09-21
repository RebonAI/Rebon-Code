// What happens when a plugin has finished draining.
//
// The host knows a plugin by its declarations and has no idea what a loader
// built behind them — a module instance, a Cordis fiber in a shared realm, a
// pool. So the loader is told when the plugin is over, and it is told before
// the terminal that lets rebon believe the drain is done: teardown that ran
// after rebon had already loaded a replacement would be disposing the new
// one's world.
import test from 'node:test';
import assert from 'node:assert/strict';
import { PluginHost } from '../src/host.mjs';

const control = (call_id, method, payload = null) => ({ protocol_version: 1, host_epoch: 7, plugin_id: '$rebon/platform', scope_id: '$rebon/control', scope_generation: 0, call_id, message: { type: 'request', method, payload } });
const scoped = (call_id, method, payload) => ({ protocol_version: 1, host_epoch: 7, plugin_id: 'plugin.a', scope_id: 'scope.a', scope_generation: 1, call_id, message: { type: 'request', method, payload } });
const answered = (sent, call_id) => sent.filter((x) => x.message.type === 'terminal').find((x) => x.call_id === call_id)?.message;

function harness() {
  const sent = [];
  const order = [];
  let release;
  const held = new Promise((resolve) => { release = resolve; });
  const writer = {
    send: async (envelope) => {
      sent.push(envelope);
      if (envelope.message.type === 'terminal') order.push(`terminal:${envelope.call_id}`);
    },
    flush: async () => {},
  };
  const serviceHandlers = new Map([
    ['quick', async () => ({ ok: true })],
    ['slow', async () => { await held; return { ok: true }; }],
  ]);
  const host = new PluginHost(writer, {
    load: async () => ({ services: [...serviceHandlers.keys()], eventTopics: [], serviceHandlers, topicHandlers: new Map() }),
    unload: async (pluginId) => { order.push(`retired:${pluginId}`); },
  });
  return { host, sent, order, release };
}

async function ready(kit) {
  await kit.host.accept(control('i', 'platform/initialize'));
  await kit.host.accept(control('l', 'plugin/load', { pluginId: 'plugin.a', root: '/pkg', entry: 'index.mjs', services: ['quick', 'slow'] }));
  await kit.host.accept(scoped('o', 'scope/open', { workspace_root: 'C:/w' }));
  return kit;
}

test('a plugin with nothing running is retired during its unload', async () => {
  const kit = await ready(harness());
  await kit.host.accept(control('u', 'plugin/unload', { pluginId: 'plugin.a' }));

  assert.deepEqual(answered(kit.sent, 'u').payload.outstandingCalls, []);
  // Retired before the unload's own terminal: rebon may act on that terminal.
  assert.deepEqual(kit.order, ['terminal:i', 'terminal:l', 'terminal:o', 'retired:plugin.a', 'terminal:u']);
});

test('a plugin with a call still running is retired when that call ends', async () => {
  const kit = await ready(harness());
  const call = kit.host.accept(scoped('s', 'service/call', { service: 'slow', request: null }));
  await kit.host.accept(control('u', 'plugin/unload', { pluginId: 'plugin.a' }));

  assert.deepEqual(answered(kit.sent, 'u').payload.outstandingCalls, ['s']);
  assert.ok(!kit.order.includes('retired:plugin.a'), 'not while work is in flight');

  kit.release();
  await call;
  // And still before the terminal of the call whose ending completed the drain.
  assert.deepEqual(kit.order.slice(-2), ['retired:plugin.a', 'terminal:s']);
});

test('retiring happens once, not once per call that ends', async () => {
  const kit = await ready(harness());
  await kit.host.accept(control('u', 'plugin/unload', { pluginId: 'plugin.a' }));
  // A second unload is refused rather than retiring again.
  await kit.host.accept(control('u2', 'plugin/unload', { pluginId: 'plugin.a' }));

  assert.equal(answered(kit.sent, 'u2').payload.code, '[STALE_PROVIDER]');
  assert.equal(kit.order.filter((entry) => entry === 'retired:plugin.a').length, 1);
});

test('a host with no loader teardown still drains', async () => {
  // The hook is optional: a plugin the built-in loader loaded has nothing to
  // take down beyond the module Node already holds.
  const sent = [];
  const host = new PluginHost({ send: async (e) => { sent.push(e); }, flush: async () => {} }, {
    load: async () => ({ services: [], eventTopics: [], serviceHandlers: new Map(), topicHandlers: new Map() }),
  });
  await host.accept(control('i', 'platform/initialize'));
  await host.accept(control('l', 'plugin/load', { pluginId: 'plugin.a', root: '/pkg', entry: 'index.mjs' }));
  await host.accept(control('u', 'plugin/unload', { pluginId: 'plugin.a' }));

  assert.equal(answered(sent, 'u').status, 'success');
});
