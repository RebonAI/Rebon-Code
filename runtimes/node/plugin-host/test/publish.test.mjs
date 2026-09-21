// `event/emit` from the host's side.
//
// The other half of the event plane. Delivery hands a plugin something it
// subscribed to; publishing takes something it wants heard. They are separate
// declarations — `eventTopics` and `publishedTopics` — because listening and
// announcing are different powers, and a plugin usually wants one of them.
import test from 'node:test';
import assert from 'node:assert/strict';
import { PluginHost } from '../src/host.mjs';
import { identityOf, terminal } from '../src/protocol.mjs';

const control = (call_id, method, payload = null) => ({ protocol_version: 1, host_epoch: 7, plugin_id: '$rebon/platform', scope_id: '$rebon/control', scope_generation: 0, call_id, message: { type: 'request', method, payload } });
const scoped = (call_id, method, generation, payload) => ({ protocol_version: 1, host_epoch: 7, plugin_id: 'plugin.a', scope_id: 'scope.a', scope_generation: generation, call_id, message: { type: 'request', method, payload } });

/// A rebon that accepts published events, or refuses them the way a real one
/// would when the topic reaches nobody.
function harness({ answer } = {}) {
  const sent = [];
  const published = [];
  const reply = answer ?? (() => ({ status: 'success', payload: { published: true } }));
  const writer = {
    send: async (envelope) => {
      sent.push(envelope);
      if (envelope.message.type !== 'request') return;
      let outcome = { status: 'success', payload: { subscribed: true } };
      if (envelope.message.method === 'event/emit') {
        published.push(envelope.message.payload);
        outcome = reply(envelope.message.payload);
      }
      queueMicrotask(() => host.accept(terminal(identityOf(envelope), outcome.status, outcome.payload)));
    },
    flush: async () => {},
  };
  const serviceHandlers = new Map([
    ['announce', async (request, ctx) => ({ answer: await ctx.publish(request.topic, request.event ?? null) })],
    ['refused', async (request, ctx) => {
      try {
        await ctx.publish(request.topic, null);
        return { refused: false };
      } catch (cause) {
        return { refused: true, code: cause.code, message: cause.message };
      }
    }],
  ]);
  const host = new PluginHost(writer, {
    load: async () => ({ services: [...serviceHandlers.keys()], eventTopics: [], serviceHandlers, topicHandlers: new Map() }),
  });
  return { host, sent, published };
}

const answered = (sent, call_id) => sent.filter((x) => x.message.type === 'terminal').find((x) => x.call_id === call_id)?.message;
const upstream = (sent) => sent.filter((x) => x.message.type === 'request');
const plainly = (value) => JSON.parse(JSON.stringify(value));

async function ready(kit, publishedTopics = ['session']) {
  await kit.host.accept(control('i', 'platform/initialize'));
  await kit.host.accept(control('l', 'plugin/load', {
    pluginId: 'plugin.a', root: '/pkg', entry: 'index.mjs',
    services: ['announce', 'refused'], publishedTopics,
  }));
  await kit.host.accept(scoped('o', 'scope/open', 1, { workspace_root: 'C:/w' }));
  return kit;
}

test('a handler publishes on a declared topic and gets rebon answer', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'announce', request: { topic: 'session', event: { n: 1 } } }));

  assert.deepEqual(plainly(kit.published), [{ topic: 'session', event: { n: 1 } }]);
  assert.deepEqual(plainly(answered(kit.sent, 's').payload), { answer: { published: true } });
});

test('a published event carries the identity the host injected', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'announce', request: { topic: 'session' } }));

  const emitted = upstream(kit.sent).find((x) => x.message.method === 'event/emit');
  assert.equal(emitted.plugin_id, 'plugin.a');
  assert.equal(emitted.scope_id, 'scope.a');
  assert.equal(emitted.scope_generation, 1);
  assert.equal(emitted.host_epoch, 7);
});

test('an undeclared topic is refused before anything is sent', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'refused', request: { topic: 'secrets' } }));

  const answer = answered(kit.sent, 's').payload;
  assert.equal(answer.refused, true);
  assert.equal(answer.code, '[UNAUTHORIZED_TOPIC]');
  assert.deepEqual(kit.published, [], 'nothing crossed the wire');
});

test('listening to a topic does not grant publishing it', async () => {
  // `eventTopics` is the listening side. A plugin that declared it and nothing
  // else may hear the topic and still not announce one.
  const kit = harness();
  await kit.host.accept(control('i', 'platform/initialize'));
  await kit.host.accept(control('l', 'plugin/load', {
    pluginId: 'plugin.a', root: '/pkg', entry: 'index.mjs',
    services: ['announce', 'refused'], eventTopics: ['session'],
  }));
  await kit.host.accept(scoped('o', 'scope/open', 1, { workspace_root: 'C:/w' }));
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'refused', request: { topic: 'session' } }));

  assert.equal(answered(kit.sent, 's').payload.code, '[UNAUTHORIZED_TOPIC]');
});

test("rebon own refusal reaches the plugin, not merely that the call failed", async () => {
  const kit = await ready(harness({
    answer: () => ({ status: 'error', payload: { code: '[UNKNOWN_TOPIC]', message: 'nobody listens to session' } }),
  }));
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'refused', request: { topic: 'session' } }));

  const answer = answered(kit.sent, 's').payload;
  assert.equal(answer.code, '[UNKNOWN_TOPIC]');
  assert.match(answer.message, /nobody listens/);
});
