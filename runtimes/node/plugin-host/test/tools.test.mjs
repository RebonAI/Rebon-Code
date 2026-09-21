// `tool/invoke` from the host's side.
//
// This is the only way a plugin reaches rebon's own capabilities, and the only
// thing a plugin ever holds that reaches back at all: the context a handler is
// given, bound to the scope incarnation the call arrived on.
import test from 'node:test';
import assert from 'node:assert/strict';
import { PluginHost } from '../src/host.mjs';
import { identityOf, terminal } from '../src/protocol.mjs';

const control = (call_id, method, payload = null) => ({ protocol_version: 1, host_epoch: 7, plugin_id: '$rebon/platform', scope_id: '$rebon/control', scope_generation: 0, call_id, message: { type: 'request', method, payload } });
const scoped = (call_id, method, generation, payload) => ({ protocol_version: 1, host_epoch: 7, plugin_id: 'plugin.a', scope_id: 'scope.a', scope_generation: generation, call_id, message: { type: 'request', method, payload } });

/// A rebon that answers `tool/invoke` however the test needs.
///
/// `answer` covers tool invocations only; subscriptions always succeed, so a
/// test about tools never fails for a reason that has nothing to do with them.
function harness({ tools = ['read_file'], answer } = {}) {
  const sent = [];
  const calls = [];
  const invoked = answer ?? ((request) => ({ status: 'success', payload: { ok: request.tool } }));
  const writer = {
    send: async (envelope) => {
      sent.push(envelope);
      if (envelope.message.type !== 'request') return;
      const { status, payload } = envelope.message.method === 'tool/invoke'
        ? invoked(envelope.message.payload)
        : { status: 'success', payload: { subscribed: true } };
      queueMicrotask(() => host.accept(terminal(identityOf(envelope), status, payload)));
    },
    flush: async () => {},
  };
  // The handler hands its context straight back so the test can inspect what a
  // plugin would actually receive.
  const serviceHandlers = new Map([
    ['use', async (request, ctx) => ({ answer: await ctx.invoke(request.tool, request.input ?? null) })],
    ['look', async (_request, ctx) => ({ scopeId: ctx.scopeId, workspaceRoot: ctx.workspaceRoot })],
    ['fail', async (request, ctx) => {
      try {
        await ctx.invoke(request.tool, null);
        return { refused: false };
      } catch (cause) {
        return { refused: true, code: cause.code, message: cause.message };
      }
    }],
    ['explode', async () => { throw new Error('deliberate'); }],
  ]);
  const topicHandlers = new Map([['session', async (_event, ctx) => { calls.push(await ctx.invoke('read_file', null)); }]]);
  const host = new PluginHost(writer, {
    load: async () => ({ services: [...serviceHandlers.keys()], eventTopics: ['session'], serviceHandlers, topicHandlers }),
  });
  return { host, sent, calls, tools };
}

const answered = (sent, call_id) => sent.filter((x) => x.message.type === 'terminal').find((x) => x.call_id === call_id)?.message;
const upstream = (sent) => sent.filter((x) => x.message.type === 'request');
// Anything that crossed the wire comes back with a null prototype: the
// protocol's clone builds objects that way so an inherited property can never
// be mistaken for payload. Strict deep-equality sees that, so a comparison
// against a literal has to normalise first.
const shape = (value) => JSON.parse(JSON.stringify(value));

async function ready(kit) {
  await kit.host.accept(control('i', 'platform/initialize'));
  await kit.host.accept(control('l', 'plugin/load', { pluginId: 'plugin.a', root: '/pkg', entry: 'index.mjs', services: ['use', 'look', 'fail', 'explode'], eventTopics: ['session'], invokableTools: kit.tools }));
  await kit.host.accept(scoped('o', 'scope/open', 1, { workspace_root: 'C:/w' }));
  return kit;
}

test('a service handler invokes a tool and gets its answer', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'use', request: { tool: 'read_file', input: { path: 'README.md' } } }));

  const invocations = upstream(kit.sent).filter((x) => x.message.method === 'tool/invoke');
  assert.equal(invocations.length, 1);
  assert.deepEqual(shape(invocations[0].message.payload), { tool: 'read_file', input: { path: 'README.md' } });
  // The identity is the host's, on the plugin's own scope incarnation — a
  // plugin cannot name a different one because it never supplies the identity.
  assert.equal(invocations[0].plugin_id, 'plugin.a');
  assert.equal(invocations[0].scope_id, 'scope.a');
  assert.equal(invocations[0].scope_generation, 1);
  assert.deepEqual(shape(answered(kit.sent, 's').payload), { answer: { ok: 'read_file' } });
});

// The manifest is the ceiling here too, and this side refuses first so the
// error points at the plugin's own line rather than at a process boundary.
test('a tool the manifest did not declare is refused before anything is sent', async () => {
  const kit = await ready(harness({ tools: ['read_file'] }));
  const before = upstream(kit.sent).length;
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'fail', request: { tool: 'write_file' } }));

  const result = answered(kit.sent, 's').payload;
  assert.equal(result.refused, true);
  assert.equal(result.code, '[UNAUTHORIZED_TOOL]');
  assert.equal(upstream(kit.sent).length, before, 'nothing was asked of rebon');
});

test('a plugin that declared no tools can invoke none', async () => {
  const kit = await ready(harness({ tools: [] }));
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'fail', request: { tool: 'read_file' } }));
  assert.equal(answered(kit.sent, 's').payload.code, '[UNAUTHORIZED_TOOL]');
});

// A denied permission is the answer a plugin author most needs to read, so the
// code has to survive the trip rather than being flattened into "it failed".
test('rebon\'s own refusal code reaches the plugin', async () => {
  const kit = await ready(harness({
    answer: () => ({ status: 'error', payload: { code: '[PERMISSION_DENIED]', message: 'the user said no' } }),
  }));
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'fail', request: { tool: 'read_file' } }));

  const result = answered(kit.sent, 's').payload;
  assert.equal(result.code, '[PERMISSION_DENIED]');
  assert.match(result.message, /the user said no/);
});

test('the context describes the scope the call arrived on', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'look', request: null }));
  assert.deepEqual(shape(answered(kit.sent, 's').payload), { scopeId: 'scope.a', workspaceRoot: 'C:/w' });
});

test('a topic handler gets the same context as a service handler', async () => {
  const kit = await ready(harness());
  const subscription = answered(kit.sent, 'o').payload.subscriptions[0];
  await kit.host.accept(scoped('d', 'event/deliver', 1, { subscription, topic: 'session', event: null }));
  assert.equal(answered(kit.sent, 'd').status, 'success');
  assert.deepEqual(shape(kit.calls), [{ ok: 'read_file' }]);
});

// One bad handler must not take the host and every other plugin down with it —
// that isolation is the reason plugins are loaded separately at all. Only the
// transport failing is fatal.
test('a handler that throws a plain error fails its own call, not the host', async () => {
  const kit = await ready(harness());
  await kit.host.accept(scoped('s', 'service/call', 1, { service: 'explode', request: null }));
  assert.equal(answered(kit.sent, 's').status, 'error');
  assert.equal(answered(kit.sent, 's').payload.code, '[HANDLER_FAILED]');
  assert.match(answered(kit.sent, 's').payload.message, /deliberate/);

  // Still answering afterwards.
  await kit.host.accept(scoped('t', 'service/call', 1, { service: 'look', request: null }));
  assert.equal(answered(kit.sent, 't').status, 'success');
});
