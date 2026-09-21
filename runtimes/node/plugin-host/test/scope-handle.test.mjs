// A handle on the session, rather than on one call inside it.
//
// Most of what a plugin does is answering: rebon calls, the plugin replies, and
// the context it replies through is the call. Some plugins also *act* — an
// agent loop produces turn events on its own schedule, a watcher notices a file
// change — and those have no inbound call to hang a message on. `plugin.scope`
// is the handle for that, and it carries exactly the powers a call context
// does minus the two that only mean something inside a call.
import test from 'node:test';
import assert from 'node:assert/strict';
import { PluginHost } from '../src/host.mjs';
import { identityOf, terminal } from '../src/protocol.mjs';

const control = (call_id, method, payload = null) => ({ protocol_version: 1, host_epoch: 7, plugin_id: '$rebon/platform', scope_id: '$rebon/control', scope_generation: 0, call_id, message: { type: 'request', method, payload } });
const scoped = (call_id, method, generation, payload) => ({ protocol_version: 1, host_epoch: 7, plugin_id: 'plugin.a', scope_id: 'scope.a', scope_generation: generation, call_id, message: { type: 'request', method, payload } });
const answered = (sent, call_id) => sent.filter((x) => x.message.type === 'terminal').find((x) => x.call_id === call_id)?.message;
const upstream = (sent, method) => sent.filter((x) => x.message.type === 'request' && x.message.method === method);

function harness({ handlers } = {}) {
  const sent = [];
  const log = [];
  const writer = {
    send: async (envelope) => {
      sent.push(envelope);
      if (envelope.message.type !== 'request') return;
      queueMicrotask(() => host.accept(terminal(identityOf(envelope), 'success', { ok: true })));
    },
    flush: async () => {},
  };
  const host = new PluginHost(writer, {
    load: async () => ({
      services: [], eventTopics: [], serviceHandlers: new Map(), topicHandlers: new Map(),
      scopeHandlers: handlers ?? [(ctx) => {
        log.push(`opened:${ctx.scopeId}:${ctx.workspaceRoot}`);
        log.push(ctx);
        return () => { log.push('closed'); };
      }],
    }),
  });
  return { host, sent, log };
}

async function ready(kit, load = {}) {
  await kit.host.accept(control('i', 'platform/initialize'));
  await kit.host.accept(control('l', 'plugin/load', {
    pluginId: 'plugin.a', root: '/pkg', entry: 'index.mjs',
    publishedTopics: ['loop:event'], seats: ['logger'], invokableTools: ['read_file'], ...load,
  }));
  await kit.host.accept(scoped('o', 'scope/open', 1, { workspace_root: 'C:/w' }));
  return kit;
}

test('a scope handle arrives when the scope opens and is undone when it closes', async () => {
  const kit = await ready(harness());
  assert.equal(kit.log[0], 'opened:scope.a:C:/w');
  await kit.host.accept(scoped('c', 'scope/close', 2, null));
  assert.equal(kit.log.at(-1), 'closed');
});

test('a scope handle publishes without any call being in flight', async () => {
  const kit = await ready(harness());
  const ctx = kit.log[1];
  const answer = await ctx.publish('loop:event', { seq: 1 });

  assert.deepEqual(JSON.parse(JSON.stringify(answer)), { ok: true });
  const [emitted] = upstream(kit.sent, 'event/emit');
  assert.equal(emitted.scope_id, 'scope.a');
  assert.equal(emitted.scope_generation, 1);
  assert.deepEqual(JSON.parse(JSON.stringify(emitted.message.payload)), { topic: 'loop:event', event: { seq: 1 } });
});

test('the same declarations gate a scope handle as gate a call context', async () => {
  const kit = await ready(harness());
  const ctx = kit.log[1];
  await assert.rejects(() => ctx.publish('secrets', null), { code: '[UNAUTHORIZED_TOPIC]' });
  await assert.rejects(() => ctx.seat('credentials', 'resolveEnv'), { code: '[UNAUTHORIZED_SEAT]' });
  await assert.rejects(() => ctx.invoke('write_file', null), { code: '[UNAUTHORIZED_TOOL]' });
  // And the declared ones go through.
  await ctx.seat('logger', 'warn', { message: 'x' });
  await ctx.invoke('read_file', { path: 'a' });
  assert.equal(upstream(kit.sent, 'seat/call').length, 1);
  assert.equal(upstream(kit.sent, 'tool/invoke').length, 1);
});

test('a handle captured past its incarnation refuses rather than speaking for it', async () => {
  const kit = await ready(harness());
  const ctx = kit.log[1];
  await kit.host.accept(scoped('c', 'scope/close', 2, null));

  await assert.rejects(() => ctx.publish('loop:event', null), { code: '[SCOPE_CLOSED]' });
  assert.equal(upstream(kit.sent, 'event/emit').length, 0);
});

test('unloading the plugin closes its scope handles', async () => {
  const kit = await ready(harness());
  await kit.host.accept(control('u', 'plugin/unload', { pluginId: 'plugin.a' }));

  assert.equal(kit.log.at(-1), 'closed');
  await assert.rejects(() => kit.log[1].publish('loop:event', null), { code: '[SCOPE_CLOSED]' });
});

test('a scope handler that throws fails the open', async () => {
  // A plugin that believes it is attached to a session and is not would be a
  // worse outcome than a scope that refused to open.
  const kit = harness({ handlers: [() => { throw new Error('cannot attach'); }] });
  await ready(kit);
  const answer = answered(kit.sent, 'o');
  assert.equal(answer.status, 'error');
  assert.match(answer.payload.message, /cannot attach/);
});

test('shutdown closes scope handles too', async () => {
  const kit = await ready(harness());
  await kit.host.accept(control('s', 'platform/shutdown'));
  assert.equal(kit.log.at(-1), 'closed');
});
