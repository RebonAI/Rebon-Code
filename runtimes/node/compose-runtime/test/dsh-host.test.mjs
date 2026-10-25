// The DeepSeek Harness Host services the realm answers itself, and the
// client contract's realm half.
//
// Upstream dsh packages inject services that, in dsh, its boot layer and its
// session model provide: `workingDirectory`, `loader`, `configEditor`,
// `profileContext`. Rebon is the boot layer and owns the session, so the
// realm answers them from its own facts. These tests hold each to the dsh
// contract a real package reads, with small local packages standing in for
// the npm ones (the npm ones are exercised end to end by rebon's
// `dsh_upstream` test).
import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadCompose, loadEntry, openScope, plainly, startHost } from './harness.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const probeScript = path.join(here, '..', 'src', 'probe.mjs');

function cordisPackage(body, packageJson) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'rebon-host-'));
  fs.writeFileSync(path.join(root, 'entry.mjs'), body);
  if (packageJson) fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify(packageJson));
  return { root: root.replace(/\\/g, '/'), entry: 'entry.mjs' };
}

const remote = (kit, endpoint, args = {}) => kit.scoped('rebon:compose', 'service/call', {
  service: 'compose', request: { kind: 'remote', endpoint, args },
});

// A tool package that places its section by dsh's name and words it after
// the tools it registered, the way `dsh-tool-fs` does.
const READER = `
import { defineTool } from '@deepseek-ai/dsh-tools';
export const name = 'reader';
export const inject = ['tools', 'systemPrompt', 'workingDirectory'];
export function apply(ctx) {
  ctx.systemPrompt.section({
    name: 'reader:guidance',
    order: ctx.systemPrompt.getSectionOrder('TOOL_READ'),
    text: ({ scope }) => ctx.tools.get('where', scope) === undefined ? '' : 'Use where to find your directory.',
  });
  ctx.systemPrompt.section({ name: 'reader:empty', order: 1, text: () => '' });
  ctx.tools.register(defineTool({
    name: 'where',
    description: 'Says the working directory.',
    parameters: {},
    output: {
      schema: { type: 'object', additionalProperties: false, properties: { cwd: { type: 'string', required: true } } },
      render: (_args, value) => [{ type: 'text', text: value.cwd }],
    },
    execute: async (_args, exec) => ({ cwd: await ctx.workingDirectory.ensure(exec.agent, exec.signal) }),
  }));
}
`;

test('a tool package reaches the session directory and words its section after its tools', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'reader' }] });
    const loaded = await loadEntry(kit, { id: 'reader', ...cordisPackage(READER), tools: ['where'], publishedTopics: ['compose:session/append'] });
    assert.equal(loaded.status, 'success', JSON.stringify(loaded.payload));
    await openScope(kit, 'reader', 'C:/projects/demo');
    const call = await kit.scoped('reader', 'tool/call', { tool: 'where', input: {} });
    assert.equal(call.terminal.status, 'success', JSON.stringify(call.terminal.payload));
    assert.equal(plainly(call.terminal.payload).content[0].text, 'C:/projects/demo');

    await openScope(kit, 'rebon:compose');
    const report = await kit.scoped('rebon:compose', 'service/call', { service: 'compose', request: { kind: 'report', pluginId: 'reader' } });
    assert.deepEqual(plainly(report.terminal.payload.sections), [
      { name: 'reader:guidance', order: 1100, text: 'Use where to find your directory.' },
    ], 'a provider text resolves once loaded, and an empty one contributes nothing');
  } finally {
    await kit.stop();
  }
});

test('the working directory cannot be changed from a plugin', async () => {
  const { RebonWorkingDirectory } = await import('../src/working-directory-runtime.mjs').then((m) => ({ RebonWorkingDirectory: m.default }));
  assert.equal(typeof RebonWorkingDirectory, 'function');
  const kit = await startHost();
  try {
    await loadCompose(kit);
    const { realm } = await import('../src/realm.mjs');
    await assert.rejects(realm().ctx.workingDirectory.set(undefined, 'C:/elsewhere'), /\[UNSUPPORTED\]/);
    assert.throws(() => realm().ctx.systemPrompt.getSectionOrder('NOT_A_PLACE'), /unknown prompt section placement/);
    assert.equal(realm().ctx.systemPrompt.getContextOrder('WORKING_DIRECTORY'), 100);
  } finally {
    await kit.stop();
  }
});

// A plugin with live-editable config, the shape `dsh-settings` forms read.
const KNOB = `
import z from '@deepseek-ai/schemastery';
export const name = 'knob';
export const Config = z.object({ level: z.number().default(1).volatile(), label: z.string().default('x') });
export function apply(ctx) {
  globalThis.__knobApplied = (globalThis.__knobApplied ?? 0) + 1;
  ctx.on('loader/volatile-update', (paths) => { globalThis.__knobPaths = paths; });
}
`;

test('the configuration editor persists an entry\'s config as that entry and applies it live', async () => {
  const kit = await startHost({ seats: { settings: (payload) => ({ entry: 'knob', changed: true, saw: payload.params }) } });
  try {
    await loadCompose(kit, { entries: [{ id: 'knob' }], profile: { home: 'C:/config/dsh' } });
    const loaded = await loadEntry(kit, { id: 'knob', ...cordisPackage(KNOB), config: { level: 2 }, seats: ['settings'] });
    assert.equal(loaded.status, 'success', JSON.stringify(loaded.payload));
    await openScope(kit, 'knob');
    const { realm } = await import('../src/realm.mjs');
    const ctx = realm().ctx;
    assert.equal(ctx.profileContext.home, 'C:/config/dsh');
    const [row] = ctx.configEditor.configuration();
    assert.equal(row.entry.id, 'knob');
    assert.deepEqual(plainly(row.override), { level: 2 });
    assert.deepEqual(plainly(row.inherited), {});
    assert.deepEqual(ctx.loader.entries().map((entry) => entry.id), ['knob']);
    await ctx.loader.await();

    await ctx.configEditor.edit(row.entry, (current) => ({ ...current, level: 7 }));
    const write = kit.seen.seats.find((call) => call.seat === 'settings');
    assert.deepEqual(plainly(write), { seat: 'settings', method: 'entryConfig', params: { config: { level: 7 } } });
    const request = kit.sent.find((envelope) => envelope.message?.method === 'seat/call' && envelope.message.payload?.seat === 'settings');
    assert.equal(request?.plugin_id, 'knob', 'the write is made as the entry whose config it is');
    assert.deepEqual(plainly(row.entry.options.config), { level: 7 });
    // Committed into the reference the plugin reads it through, without a
    // remount, and announced to that plugin.
    assert.equal(row.entry.fiber.config.level.get(), 7);
    assert.equal(globalThis.__knobApplied, 1);
    assert.deepEqual(globalThis.__knobPaths, [['level']]);

    // An ordinary field is persisted and waits for rebon to load the entry
    // again: a plugin restarted in place could not re-register.
    await ctx.configEditor.edit(row.entry, (current) => ({ ...current, label: 'y' }));
    assert.deepEqual(plainly(row.entry.options.config), { level: 7, label: 'y' });
    assert.equal(row.entry.fiber.config.label, 'x');
    assert.equal(globalThis.__knobApplied, 1);

    // A config the plugin's own schema refuses is refused before anything
    // is written.
    const before = kit.seen.seats.length;
    await assert.rejects(ctx.configEditor.edit(row.entry, () => ({ level: 'loud' })));
    assert.equal(kit.seen.seats.length, before);
  } finally {
    await kit.stop();
  }
});
