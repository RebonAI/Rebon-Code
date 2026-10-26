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

// A shell executor's use of the seam: spawn, collect, wait for the outcome.
const RUNNER = `
import { defineTool } from '@deepseek-ai/dsh-tools';
export const name = 'runner';
export const inject = ['tools', 'subprocess'];
export function apply(ctx) {
  ctx.tools.register(defineTool({
    name: 'run_it',
    description: 'Runs a program.',
    parameters: {},
    output: {
      schema: { type: 'object', additionalProperties: true, properties: {} },
      render: (_args, value) => [{ type: 'text', text: JSON.stringify(value) }],
    },
    execute: async (_args, exec) => {
      const handle = ctx.subprocess.spawn({
        argv: ['pwsh', '-Command', 'Write-Output hi'],
        cwd: 'C:/projects/demo',
        stdio: { stdin: { data: 'in' }, stdout: { maxBytes: 4 }, stderr: { maxBytes: 100 } },
        graceMs: 100,
        signal: exec.signal,
        env: { DSH_SESSION: 's1', DROPPED: undefined },
      });
      const outcome = await handle.done;
      return {
        outcome,
        stdout: handle.collected.stdout.readFrom(0),
        stderr: handle.collected.stderr.readFrom(0),
        tail: handle.collected.stdout.readFrom(9),
        exited: await handle.waitForExit(),
      };
    },
  }));
  let refusal;
  try {
    ctx.subprocess.spawn({ argv: ['x'], stdio: { stdin: 'ignore', stdout: 'pipe', stderr: { maxBytes: 1 } } });
  } catch (error) { refusal = error.code; }
  globalThis.__pipeRefusal = refusal;
}
`;

test('a program is started by rebon through the process seat, as the dsh seam asks', async () => {
  const calls = [];
  const answers = [
    { stdout: { text: 'hello', dropped: 0 }, stderr: { text: '', dropped: 0 }, exit: null },
    { stdout: { text: ' world', dropped: 3 }, stderr: { text: 'warn', dropped: 0 }, exit: { exitCode: 0, signal: null } },
  ];
  const kit = await startHost({ seats: { process: (payload) => {
    calls.push({ method: payload.method, params: payload.params });
    if (payload.method === 'spawn') return { id: 7 };
    return answers.shift();
  } } });
  try {
    await loadCompose(kit, { entries: [{ id: 'runner' }] });
    const loaded = await loadEntry(kit, { id: 'runner', ...cordisPackage(RUNNER), tools: ['run_it'], seats: ['process'], publishedTopics: ['compose:session/append'] });
    assert.equal(loaded.status, 'success', JSON.stringify(loaded.payload));
    assert.equal(globalThis.__pipeRefusal, '[UNSUPPORTED]', 'a raw pipe is refused by name');
    await openScope(kit, 'runner');
    const call = await kit.scoped('runner', 'tool/call', { tool: 'run_it', input: {} });
    assert.equal(call.terminal.status, 'success', JSON.stringify(call.terminal.payload));
    const value = JSON.parse(plainly(call.terminal.payload).content[0].text);
    assert.deepEqual(value.outcome, { exitCode: 0, signal: null });
    // Four bytes kept of a fourteen-byte stream (three of them dropped on
    // rebon's side): a read from the start is lossy and gets the tail.
    assert.deepEqual(value.stdout, { text: 'orld', nextOffset: 14, lossy: true });
    assert.deepEqual(value.tail, { text: 'orld', nextOffset: 14, lossy: true });
    assert.deepEqual(value.stderr, { text: 'warn', nextOffset: 4, lossy: false });
    assert.equal(value.exited, true);
    assert.deepEqual(plainly(calls[0]), {
      method: 'spawn',
      params: { argv: ['pwsh', '-Command', 'Write-Output hi'], cwd: 'C:/projects/demo', env: { DSH_SESSION: 's1', DROPPED: null }, stdin: 'in' },
    });
    assert.deepEqual(calls.slice(1).map((c) => c.method), ['wait', 'wait']);
    assert.equal(plainly(calls[1]).params.id, 7);
  } finally {
    await kit.stop();
  }
});

test('subprocess collection does not join bytes across a gap or retain an unbounded zero-sized tail', async () => {
  for (const maxBytes of [64, 0]) {
    const answers = [
      { stdout: { text: 'hello', dropped: 0 }, exit: null },
      { stdout: { text: ' world', dropped: 3 }, exit: { exitCode: 0, signal: null } },
    ];
    const kit = await startHost({ seats: { process: (payload) => payload.method === 'spawn' ? { id: 7 } : answers.shift() } });
    try {
      await loadCompose(kit, { entries: [{ id: 'runner' }] });
      const body = RUNNER.replace('stdout: { maxBytes: 4 }', `stdout: { maxBytes: ${maxBytes} }`);
      const loaded = await loadEntry(kit, { id: 'runner', ...cordisPackage(body), tools: ['run_it'], seats: ['process'] });
      assert.equal(loaded.status, 'success', JSON.stringify(loaded.payload));
      await openScope(kit, 'runner');
      const call = await kit.scoped('runner', 'tool/call', { tool: 'run_it', input: {} });
      assert.equal(call.terminal.status, 'success', JSON.stringify(call.terminal.payload));
      const value = JSON.parse(plainly(call.terminal.payload).content[0].text);
      // 中间丢失三字节，不能把旧前缀接到新尾部并宣称完整。
      assert.deepEqual(value.stdout, { text: maxBytes === 0 ? '' : ' world', nextOffset: 14, lossy: true });
    } finally {
      await kit.stop();
    }
  }
});

test('subprocess waitForExit observes an already aborted wait without losing the live outcome', async () => {
  const answers = [{ exit: { exitCode: 0, signal: null } }];
  const kit = await startHost({ seats: { process: (payload) => payload.method === 'spawn' ? { id: 7 } : answers.shift() } });
  try {
    await loadCompose(kit, { entries: [{ id: 'runner' }] });
    const body = RUNNER.replace('const outcome = await handle.done;', `
      const controller = new AbortController();
      controller.abort();
      const wait = handle.waitForExit(controller.signal);
      const outcome = await handle.done;
      if (await wait !== false) throw new Error('aborted wait did not return false');
    `);
    const loaded = await loadEntry(kit, { id: 'runner', ...cordisPackage(body), tools: ['run_it'], seats: ['process'] });
    assert.equal(loaded.status, 'success', JSON.stringify(loaded.payload));
    await openScope(kit, 'runner');
    const call = await kit.scoped('runner', 'tool/call', { tool: 'run_it', input: {} });
    assert.equal(call.terminal.status, 'success', JSON.stringify(call.terminal.payload));
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

test('the client contract answers from the realm in dsh\'s wire shape', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'brand' }] });
    const client = cordisPackage('export const name = "brand";\nexport function apply() {}\n', {
      name: '@deepseek-ai/dsh-client-ui-brand-official',
      exports: { '.': './entry.mjs', './client': './lib/client.js' },
      dsh: { client: { platform: 'web', inject: ['@deepseek-ai/dsh-client-ui-slots'] } },
    });
    fs.mkdirSync(path.join(client.root, 'lib'));
    fs.writeFileSync(path.join(client.root, 'lib', 'client.js'), 'window.__ModuleLoader__.load({ id: "brand", factory: () => ({}) });\n');
    assert.equal((await loadEntry(kit, { id: 'brand', ...client })).status, 'success');
    await openScope(kit, 'rebon:compose');

    const inventory = await remote(kit, 'pluginInventory/list');
    assert.deepEqual(plainly(inventory.terminal.payload), {
      ok: true,
      value: {
        managementAvailable: false,
        entries: [{ entryId: 'brand', moduleName: '@deepseek-ai/dsh-client-ui-brand-official', enabled: true, fiberPhase: 'active' }],
      },
    });

    const modules = plainly((await remote(kit, 'rebon/clientModules')).terminal.payload);
    assert.equal(modules.ok, true);
    assert.equal(modules.value.entries.length, 1);
    const [row] = modules.value.entries;
    assert.equal(row.entryId, 'brand');
    assert.equal(row.id, '@deepseek-ai/dsh-client-ui-brand-official');
    assert.equal(path.resolve(row.file), path.resolve(client.root, 'lib', 'client.js'));
    assert.deepEqual(row.inject, ['@deepseek-ai/dsh-client-ui-slots']);
    assert.match(row.rev, /^\d+-\d+$/);

    // 无效的 wire 参数不应被当作空命名参数接受。
    const invalid = plainly((await remote(kit, 'pluginInventory/list', [])).terminal.payload);
    assert.equal(invalid.error.code, 'gateway/bad-request');

    // No settings service in this composition: unavailable, by name.
    const settings = plainly((await remote(kit, 'settings/describe')).terminal.payload);
    assert.equal(settings.ok, false);
    assert.equal(settings.error.code, 'gateway/method-unavailable');

    const unknown = plainly((await remote(kit, 'session/list')).terminal.payload);
    assert.deepEqual(unknown.error.code, 'gateway/method-unavailable');

    // Credentials are the plane's to describe, not a realm's.
    const credentials = plainly((await remote(kit, 'credentials/describe', { refs: ['EXA_API_KEY'] })).terminal.payload);
    assert.equal(credentials.error.code, 'gateway/method-unavailable');
  } finally {
    await kit.stop();
  }
});

test('realm remote events use the control scope and stop publishing after it closes', { timeout: 5000 }, async () => {
  let received;
  const heard = new Promise((resolve) => { received = resolve; });
  const kit = await startHost({ onEvent: received });
  try {
    await loadCompose(kit);
    await openScope(kit, 'rebon:compose');
    const { realm } = await import('../src/realm.mjs');
    realm().ctx.emit('commands/change', { revision: 1 });
    // 等待实际 wire 发布，不靠 sleep 或微任务次数猜测异步桥何时完成。
    await heard;
    assert.deepEqual(plainly(kit.seen.events), [{
      topic: 'compose:remote-event',
      pluginId: 'rebon:compose',
      event: { event: 'commands/change', args: [{ revision: 1 }] },
    }]);
    const closed = await kit.scoped('rebon:compose', 'scope/close', null, 2);
    assert.equal(closed.terminal.status, 'success', JSON.stringify(closed.terminal.payload));
    realm().ctx.emit('commands/change', { revision: 2 });
    assert.equal(kit.seen.events.length, 1, '关闭的 scope 不再发布 Host 事件');
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
      { id: 'tools-again', name: '@demo/tools-service' },
      { id: 'off', name: '@demo/provider', disabled: true },
      { id: 'sometimes', name: '@demo/provider', disabled: { __jsExpr: "process.platform === 'win32'" } },
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
  // dsh's own `tools` service, which rebon's seat already is.
  write('node_modules/@demo/tools-service/package.json', JSON.stringify({ name: '@demo/tools-service', type: 'module', main: 'index.js' }));
  write('node_modules/@demo/tools-service/index.js', `
import { Service } from 'cordis';
export default class Tools extends Service { constructor(ctx) { super(ctx, 'tools'); } }
`);
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
    off: '[DISABLED]',
    sometimes: '[CONFIG_EXPRESSION]',
    'tools-again': '[REBON_PROVIDES]',
    stranded: '[MISSING_INJECT]',
  });
  assert.deepEqual(answer.skipped.find((row) => row.id === 'stranded').missing, ['agents']);
});
