// A composition, one `plugin/load` at a time.
//
// The shape the plane takes: every Cordis entry is its own plugin, with its own
// declarations and its own drain, and what makes them a composition rather than
// a pile is the realm they share. These tests are about that seam — that an
// entry loads, registers only what it declared, reaches the entries loaded
// before it, and takes exactly its own registrations with it when it unloads.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { loadCompose, loadEntry, openScope, plainly, startHost } from './harness.mjs';

/// Writes a small Cordis plugin package and returns its load coordinates.
function cordisPackage(body) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'rebon-entry-'));
  fs.writeFileSync(path.join(root, 'entry.mjs'), body);
  return { root: root.replace(/\\/g, '/'), entry: 'entry.mjs' };
}

const TOOLBOX = `
import { defineTool } from '@deepseek-ai/dsh-tools';
export const name = 'toolbox';
export const inject = ['tools'];
export function apply(ctx, config) {
  ctx.tools.register(defineTool({
    name: config?.toolName ?? 'echo_tool',
    description: 'Echoes its argument back.',
    parameters: { text: { type: 'string', required: true, description: 'What to echo.' } },
    output: {
      schema: { type: 'object', additionalProperties: false, properties: { text: { type: 'string', required: true } } },
      render: (_args, value) => [{ type: 'text', text: String(value.text) }],
    },
    execute: async (args, exec) => {
      exec.agent.session.append('echo', { text: args.text });
      return { text: args.text };
    },
  }));
}
`;

// Cordis's `inject` is an ACL, not documentation: a plugin can only reach the
// services it names, which is as true of rebon's seats as of any other.
const CONSUMER = `
export const name = 'consumer';
export const inject = ['tools', 'systemPrompt'];
export function apply(ctx) {
  ctx.systemPrompt.section({
    name: 'consumer',
    order: 10,
    text: 'tools visible to me: ' + [...ctx.tools.defs.keys()].join(','),
  });
}
`;

test('an entry loads as its own plugin and reports what it registered', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'toolbox' }] });
    const pkg = cordisPackage(TOOLBOX);
    const load = await loadEntry(kit, { id: 'toolbox', ...pkg, tools: ['echo_tool'], publishedTopics: ['compose:session/append'] });

    assert.equal(load.status, 'success', JSON.stringify(load.payload));
    assert.equal(load.payload.tools.length, 1);
    const [tool] = plainly(load.payload.tools);
    assert.equal(tool.name, 'echo_tool');
    assert.equal(tool.description, 'Echoes its argument back.');
    // dsh's own schema layer compiled the parameters; the plane carries what it
    // produced rather than re-deriving it.
    assert.deepEqual(tool.inputSchema.required, ['text']);
    assert.equal(tool.inputSchema.properties.text.type, 'string');
  } finally {
    await kit.stop();
  }
});

test('a tool the entry did not declare is refused where the plugin registered it', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'toolbox' }] });
    const pkg = cordisPackage(TOOLBOX);
    const load = await loadEntry(kit, { id: 'toolbox', ...pkg, tools: ['something_else'] });

    assert.equal(load.status, 'error');
    assert.equal(load.payload.code, '[UNAUTHORIZED_REGISTER]');
    assert.match(load.payload.message, /echo_tool/);
  } finally {
    await kit.stop();
  }
});

test('rebon calls a composition tool, and the tool publishes what it did', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'toolbox' }] });
    const pkg = cordisPackage(TOOLBOX);
    await loadEntry(kit, { id: 'toolbox', ...pkg, tools: ['echo_tool'], publishedTopics: ['compose:session/append'] });
    await openScope(kit, 'toolbox');

    const call = await kit.scoped('toolbox', 'tool/call', { tool: 'echo_tool', input: { text: '你好' } });
    assert.equal(call.terminal.status, 'success', JSON.stringify(call.terminal.payload));
    assert.deepEqual(plainly(call.terminal.payload), { content: [{ type: 'text', text: '你好' }], isError: false });

    assert.equal(kit.seen.events.length, 1);
    assert.equal(kit.seen.events[0].topic, 'compose:session/append');
    assert.equal(kit.seen.events[0].pluginId, 'toolbox');
    assert.deepEqual(plainly(kit.seen.events[0].event.data), { text: '你好' });
  } finally {
    await kit.stop();
  }
});

test('the realm is shared: an entry sees what earlier entries registered', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'toolbox' }, { id: 'consumer' }] });
    await loadEntry(kit, { id: 'toolbox', ...cordisPackage(TOOLBOX), tools: ['echo_tool'], publishedTopics: ['compose:session/append'] });
    await loadEntry(kit, { id: 'consumer', ...cordisPackage(CONSUMER) });
    await openScope(kit, 'rebon:compose');

    const report = await kit.scoped('rebon:compose', 'service/call', { service: 'compose', request: { kind: 'report', pluginId: 'consumer' } });
    assert.equal(report.terminal.status, 'success', JSON.stringify(report.terminal.payload));
    // The consumer read the tools table at mount time; that it saw the
    // toolbox's tool is the shared realm working.
    assert.deepEqual(plainly(report.terminal.payload.sections), [
      { name: 'consumer', order: 10, text: 'tools visible to me: echo_tool' },
    ]);
  } finally {
    await kit.stop();
  }
});

test('unloading one entry takes its registrations and leaves the rest', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'a' }, { id: 'b' }] });
    await loadEntry(kit, { id: 'a', ...cordisPackage(TOOLBOX), config: { toolName: 'tool_a' }, tools: ['tool_a'], publishedTopics: ['compose:session/append'] });
    await loadEntry(kit, { id: 'b', ...cordisPackage(TOOLBOX), config: { toolName: 'tool_b' }, tools: ['tool_b'], publishedTopics: ['compose:session/append'] });
    await openScope(kit, 'a');
    await openScope(kit, 'b');

    const drain = await kit.control('plugin/unload', { pluginId: 'a' });
    assert.equal(drain.status, 'success');
    assert.deepEqual(drain.payload.outstandingCalls, []);

    // a's tool is gone from the composition; b's still answers.
    const gone = await kit.scoped('a', 'tool/call', { tool: 'tool_a', input: { text: 'x' } });
    assert.equal(gone.terminal.payload.code, '[STALE_PROVIDER]');
    const alive = await kit.scoped('b', 'tool/call', { tool: 'tool_b', input: { text: 'y' } });
    assert.equal(alive.terminal.status, 'success');

    const list = await kit.scoped('rebon:compose', 'service/call', { service: 'compose', request: { kind: 'list' } }, 1);
    assert.deepEqual(plainly(list.terminal.payload.plugins), ['b']);
  } finally {
    await kit.stop();
  }
});

test('a group is structure, not a module, and refuses to be loaded', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'loop', group: [{ id: 'inner' }] }] });
    const load = await loadEntry(kit, { id: 'loop', ...cordisPackage(TOOLBOX), tools: ['echo_tool'] });

    assert.equal(load.status, 'error');
    assert.equal(load.payload.code, '[GROUP_NOT_LOADABLE]');
  } finally {
    await kit.stop();
  }
});

test('isolate gives a group its own realm of a service', async () => {
  // The agent loop needs this: the real dsh systemPrompt and rebon's seat share
  // a name, and without isolation one of them loses. The proof is that the same
  // module loads outside the group and cannot inside it — inside, the name
  // resolves in a realm nothing has provided yet.
  const REPORTER = `
export const name = 'reporter';
export const inject = ['systemPrompt'];
export function apply(ctx) {
  ctx.systemPrompt.section({ name: 'reporter', order: 1, text: 'seat reachable' });
}
`;
  const kit = await startHost();
  try {
    await loadCompose(kit, {
      entries: [
        { id: 'group', isolate: { systemPrompt: 'loop' }, group: [{ id: 'inside' }] },
        { id: 'outside' },
      ],
    });
    const inside = await loadEntry(kit, { id: 'inside', ...cordisPackage(REPORTER) });
    assert.equal(inside.status, 'error');
    assert.equal(inside.payload.code, '[MISSING_INJECT]');
    assert.match(inside.payload.message, /systemPrompt/);

    const outside = await loadEntry(kit, { id: 'outside', ...cordisPackage(REPORTER) });
    assert.equal(outside.status, 'success', JSON.stringify(outside.payload));
    await openScope(kit, 'rebon:compose');
    const report = await kit.scoped('rebon:compose', 'service/call', { service: 'compose', request: { kind: 'report', pluginId: 'outside' } });
    assert.deepEqual(plainly(report.terminal.payload.sections), [
      { name: 'reporter', order: 1, text: 'seat reachable' },
    ]);
  } finally {
    await kit.stop();
  }
});

test('a required service nothing provides is a named refusal, not a hung load', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'needy' }] });
    const load = await loadEntry(kit, { id: 'needy', ...cordisPackage(`
export const name = 'needy';
export const inject = ['nothing-provides-this'];
export function apply() {}
`) });
    assert.equal(load.status, 'error');
    assert.equal(load.payload.code, '[MISSING_INJECT]');
  } finally {
    await kit.stop();
  }
});

test('an entry that throws while mounting fails its own load and nothing else', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'bad' }, { id: 'good' }] });
    const bad = await loadEntry(kit, { id: 'bad', ...cordisPackage("export function apply(){ throw new Error('boom'); }\n") });
    assert.equal(bad.status, 'error');
    assert.match(bad.payload.message, /boom/);

    const good = await loadEntry(kit, { id: 'good', ...cordisPackage(TOOLBOX), tools: ['echo_tool'], publishedTopics: ['compose:session/append'] });
    assert.equal(good.status, 'success', JSON.stringify(good.payload));
  } finally {
    await kit.stop();
  }
});

test('an entry with no realm yet is refused by name', async () => {
  const kit = await startHost();
  try {
    const load = await loadEntry(kit, { id: 'toolbox', ...cordisPackage(TOOLBOX), tools: ['echo_tool'] });
    assert.equal(load.status, 'error');
    assert.equal(load.payload.code, '[NO_REALM]');
  } finally {
    await kit.stop();
  }
});
