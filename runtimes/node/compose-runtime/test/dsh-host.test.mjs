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

// ---- the bundle probe ---------------------------------------------------

function writeBundle() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'rebon-bundle-'));
  const write = (relative, text) => {
    const file = path.join(root, relative);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, text);
  };
  write('package.json', JSON.stringify({ name: 'demo-bundle', dsh: { bundle: { patch: './cordis.patch.yml' } } }));
  // The patch is dsh's: YAML rows, parsed with the `yaml` package the bundle
  // installs. A tiny stand-in answers the one call the probe makes.
  write('node_modules/yaml/package.json', JSON.stringify({ name: 'yaml', main: 'index.js' }));
  write('node_modules/yaml/index.js', `exports.parse = (text) => JSON.parse(text);\n`);
  write('cordis.patch.yml', JSON.stringify([
    { insert: [
      { id: 'consumer', name: '@demo/consumer', config: { greeting: 'hi' } },
      { id: 'stranded', name: '@demo/stranded' },
      { id: 'provider', name: '@demo/provider' },
      { id: 'library', name: '@demo/library' },
      { id: 'scripted', name: '@demo/provider', config: { root: { __jsExpr: "dshHomePath('x')" } } },
      { id: 'absent', name: '@demo/absent' },
    ] },
    { id: 'consumer', config: { greeting: 'hello' } },
  ]));
  write('node_modules/@demo/provider/package.json', JSON.stringify({ name: '@demo/provider', type: 'module', main: 'lib/index.js' }));
  write('node_modules/@demo/provider/lib/index.js', `
import { Service } from 'cordis';
export default class Store extends Service { constructor(ctx) { super(ctx, 'store'); } }
`);
  write('node_modules/@demo/consumer/package.json', JSON.stringify({ name: '@demo/consumer', type: 'module', exports: { '.': { default: './index.js' } } }));
  write('node_modules/@demo/consumer/index.js', `
import { defineTool } from '@deepseek-ai/dsh-tools';
export const name = 'consumer';
export const inject = ['tools', 'store'];
export function apply(ctx) {
  ctx.tools.register(defineTool({
    name: 'stored', description: 'x', parameters: {},
    output: { schema: { type: 'object', additionalProperties: false, properties: {} }, render: () => [] },
    execute: async () => ({}),
  }));
}
`);
  write('node_modules/@demo/stranded/package.json', JSON.stringify({ name: '@demo/stranded', type: 'module', main: 'index.js' }));
  write('node_modules/@demo/stranded/index.js', `export const name = 'stranded';\nexport const inject = ['agents'];\nexport function apply() {}\n`);
  write('node_modules/@demo/library/package.json', JSON.stringify({ name: '@demo/library', type: 'module', main: 'index.js' }));
  write('node_modules/@demo/library/index.js', 'export const helper = 1;\n');
  return fs.realpathSync(root);
}

function probeBundle(root) {
  const runtime = path.resolve(here, '..', '..');
  const args = ['--permission', `--allow-fs-read=${runtime}`, `--allow-fs-read=${root}`, probeScript, '--bundle', root];
  return new Promise((resolve, reject) => {
    execFile(process.execPath, args, { timeout: 30000 }, (error, stdout, stderr) => {
      if (error && !stdout) return reject(new Error(`${error.message}\n${stderr}`));
      resolve(JSON.parse(stdout.trim().split('\n').pop()));
    });
  });
}

test('a bundle\'s rows mount in one realm in the order their injections allow', async () => {
  const answer = await probeBundle(writeBundle());
  assert.equal(answer.ok, true, JSON.stringify(answer));
  assert.deepEqual(answer.entries.map((entry) => entry.id), ['provider', 'consumer'], JSON.stringify(answer.skipped));
  const [provider, consumer] = answer.entries;
  assert.deepEqual(provider.provides, ['store']);
  assert.equal(provider.entry, 'node_modules/@demo/provider/lib/index.js');
  assert.equal(consumer.entry, 'node_modules/@demo/consumer/index.js');
  assert.deepEqual(consumer.inject.required, ['tools', 'store']);
  assert.deepEqual(consumer.tools.map((tool) => tool.name), ['stored']);
  assert.deepEqual(consumer.config, { greeting: 'hello' }, 'a later patch row replaces the config');
  const skipped = Object.fromEntries(answer.skipped.map((row) => [row.id, row.code]));
  assert.deepEqual(skipped, {
    scripted: '[CONFIG_EXPRESSION]',
    absent: '[NOT_INSTALLED]',
    stranded: '[MISSING_INJECT]',
  });
  assert.deepEqual(answer.skipped.find((row) => row.id === 'stranded').missing, ['agents']);
});
