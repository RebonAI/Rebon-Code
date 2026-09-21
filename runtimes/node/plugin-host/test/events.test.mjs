// The event plane, from the host's side.
//
// Two directions meet here. A plugin's topic handler is a wish; a subscription
// is that wish bound to one scope incarnation, registered by asking rebon
// (`event/subscribe`, Node → rebon). Delivery comes back the other way
// (`event/deliver`, rebon → Node) and is refused whenever the subscription no
// longer describes what is being delivered.
import test from 'node:test';
import assert from 'node:assert/strict';
import { PluginHost } from '../src/host.mjs';
import { identityOf, terminal } from '../src/protocol.mjs';

const control = (call_id, method, payload = null) => ({ protocol_version: 1, host_epoch: 7, plugin_id: '$rebon/platform', scope_id: '$rebon/control', scope_generation: 0, call_id, message: { type: 'request', method, payload } });
const scoped = (call_id, method, generation, payload, over = {}) => ({ protocol_version: 1, host_epoch: 7, plugin_id: 'plugin.a', scope_id: 'scope.a', scope_generation: generation, call_id, message: { type: 'request', method, payload }, ...over });

/// A transport that answers whatever the host asks, the way a live supervisor
/// would. Without this the host's `scope/open` would wait forever, which is
/// itself the property the concurrency test below pins.
function harness({ topics = ['session'], services = [], answer = () => ({ subscribed: true }), status = 'success' } = {}) {
  const sent = [];
  const seen = [];
  const held = [];
  const writer = {
    send: async (envelope) => {
      sent.push(envelope);
      if (envelope.message.type !== 'request') return;
      const reply = terminal(identityOf(envelope), status, answer(envelope));
      queueMicrotask(() => host.accept(reply));
    },
    flush: async () => { writer.flushed = true; },
  };
  const topicHandlers = new Map(topics.map((topic) => [topic, async (event) => { seen.push([topic, event]); }]));
  const serviceHandlers = new Map(services.map((service) => [service, () => new Promise((resolve) => held.push(resolve))]));
  const host = new PluginHost(writer, {
    load: async () => ({ services, eventTopics: topics, serviceHandlers, topicHandlers }),
  });
  return { host, sent, seen, held, writer };
}

const terminals = (sent) => sent.filter((x) => x.message.type === 'terminal');
const answered = (sent, call_id) => terminals(sent).find((x) => x.call_id === call_id)?.message;

async function ready(kit, { generation = 1 } = {}) {
  await kit.host.accept(control('i', 'platform/initialize'));
  await kit.host.accept(control('l', 'plugin/load', { pluginId: 'plugin.a', root: '/pkg', entry: 'index.mjs', services: [], eventTopics: ['session'] }));
  await kit.host.accept(scoped('o', 'scope/open', generation, { workspace_root: 'C:/w' }));
  return kit;
}

test('opening a scope registers the plugin\'s topics with rebon', async () => {
  const kit = await ready(harness());
  const upstream = kit.sent.filter((x) => x.message.type === 'request');
  assert.equal(upstream.length, 1);
  assert.equal(upstream[0].message.method, 'event/subscribe');
  assert.equal(upstream[0].message.payload.topic, 'session');
  // The call carries the plugin's own identity on the incarnation being opened,
  // not the platform control identity: rebon pins the subscription to it.
  assert.equal(upstream[0].plugin_id, 'plugin.a');
  assert.equal(upstream[0].scope_generation, 1);
  assert.equal(answered(kit.sent, 'o').status, 'success');
  assert.deepEqual(answered(kit.sent, 'o').payload.subscriptions, [upstream[0].message.payload.subscription]);
});

// A plugin that registered no topics asks for nothing, so opening a scope must
// not take an upstream round trip it does not need.
test('a plugin with no topics subscribes to nothing', async () => {
  const kit = await ready(harness({ topics: [] }));
  assert.equal(kit.sent.filter((x) => x.message.type === 'request').length, 0);
  assert.equal(answered(kit.sent, 'o').status, 'success');
});

// If rebon refuses the subscription, the scope did not open the way it claims.
// Reporting success would leave a plugin believing it is listening.
test('a refused subscription fails the scope open that asked for it', async () => {
  const kit = harness({ status: 'error', answer: () => ({ code: '[UNKNOWN_TOPIC]', message: 'no such topic' }) });
  await kit.host.accept(control('i', 'platform/initialize'));
  await kit.host.accept(control('l', 'plugin/load', { pluginId: 'plugin.a', root: '/pkg', entry: 'index.mjs', eventTopics: ['session'] }));
  await kit.host.accept(scoped('o', 'scope/open', 1, { workspace_root: 'C:/w' }));
  assert.equal(answered(kit.sent, 'o').status, 'error');
  // rebon's own refusal code survives the trip, rather than being flattened
  // into "the call failed".
  assert.equal(answered(kit.sent, 'o').payload.code, '[UNKNOWN_TOPIC]');
});

test('a delivery reaches the topic handler and is acknowledged', async () => {
  const kit = await ready(harness());
  const subscription = answered(kit.sent, 'o').payload.subscriptions[0];
  await kit.host.accept(scoped('d', 'event/deliver', 1, { subscription, topic: 'session', event: { n: 1 } }));
  assert.deepEqual(kit.seen, [['session', { n: 1 }]]);
  assert.equal(answered(kit.sent, 'd').status, 'success');
  assert.equal(answered(kit.sent, 'd').payload.delivered, true);
});

test('a delivery naming no live subscription is refused before any handler runs', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('d', 'event/deliver', 1, { subscription: 'sub-nope', topic: 'session', event: null }));
  assert.equal(answered(kit.sent, 'd').payload.code, '[UNKNOWN_SUBSCRIPTION]');
  assert.deepEqual(kit.seen, []);
});

// The subscription's topic is what it was registered for. A delivery claiming a
// different one is either a bug or a plugin being handed events it never asked
// for; both are refusals.
test('a delivery on the wrong topic for its subscription is refused', async () => {
  const kit = await ready(harness());
  const subscription = answered(kit.sent, 'o').payload.subscriptions[0];
  await kit.host.accept(scoped('d', 'event/deliver', 1, { subscription, topic: 'other', event: null }));
  assert.equal(answered(kit.sent, 'd').payload.code, '[TOPIC_MISMATCH]');
  assert.deepEqual(kit.seen, []);
});

// The one refusal that matters most: running the handler would hand the plugin
// an event from a session it has already finished.
test('a delivery on a past scope incarnation is refused', async () => {
  const kit = await ready(harness());
  const subscription = answered(kit.sent, 'o').payload.subscriptions[0];
  await kit.host.accept(scoped('d', 'event/deliver', 2, { subscription, topic: 'session', event: null }));
  assert.equal(answered(kit.sent, 'd').payload.code, '[STALE_SUBSCRIPTION]');
  assert.deepEqual(kit.seen, []);
});

test('closing a scope revokes what it held and later delivery finds nothing', async () => {
  const kit = await ready(harness());
  const subscription = answered(kit.sent, 'o').payload.subscriptions[0];
  await kit.host.accept(scoped('c', 'scope/close', 2, null));
  assert.deepEqual(answered(kit.sent, 'c').payload.revokedSubscriptions, [subscription]);
  await kit.host.accept(scoped('d', 'event/deliver', 2, { subscription, topic: 'session', event: null }));
  assert.equal(answered(kit.sent, 'd').payload.code, '[UNKNOWN_SUBSCRIPTION]');
});

test('unloading a plugin revokes its subscriptions and reports them', async () => {
  const kit = await ready(harness());
  const subscription = answered(kit.sent, 'o').payload.subscriptions[0];
  await kit.host.accept(control('u', 'plugin/unload', { pluginId: 'plugin.a' }));
  assert.deepEqual(answered(kit.sent, 'u').payload.revokedSubscriptions, [subscription]);
  await kit.host.accept(scoped('d', 'event/deliver', 1, { subscription, topic: 'session', event: null }));
  assert.equal(answered(kit.sent, 'd').payload.code, '[UNKNOWN_SUBSCRIPTION]');
});

// The property the concurrent read loop exists for. A handler that is waiting
// on something must not stop the host from handling anything else — if it did,
// a handler waiting on rebon's answer would be waiting on a frame the host
// cannot read until the handler returns.
test('a request still running does not block the next one', async () => {
  const kit = await ready(harness({ services: ['slow'] }));
  const pending = kit.host.accept(scoped('s', 'service/call', 1, { service: 'slow', request: null }));
  assert.equal(answered(kit.sent, 's'), undefined, 'the slow call has not answered');

  const subscription = answered(kit.sent, 'o').payload.subscriptions[0];
  await kit.host.accept(scoped('d', 'event/deliver', 1, { subscription, topic: 'session', event: { n: 2 } }));
  assert.equal(answered(kit.sent, 'd').status, 'success', 'the delivery went through while the call was still running');

  kit.held[0]({ done: true });
  await pending;
  assert.equal(answered(kit.sent, 's').status, 'success');
});

// A terminal that answers nothing is not something to guess about: matching it
// to any pending call would settle the wrong promise.
test('a terminal for a call this host never made is refused', async () => {
  const kit = await ready(harness());
  await assert.rejects(
    kit.host.accept(terminal({ host_epoch: 7, plugin_id: 'plugin.a', scope_id: 'scope.a', scope_generation: 1, call_id: 'ghost' }, 'success', null)),
    { code: 'unknown_call' },
  );
});
