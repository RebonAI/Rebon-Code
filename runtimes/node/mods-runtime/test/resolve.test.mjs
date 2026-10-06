import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createLoader, MARKER } from '../src/index.mjs';
import { resolveModFile } from '../src/transpile.mjs';

const fixtures = path.join(path.dirname(fileURLToPath(import.meta.url)), 'fixtures');

function tree(files) {
  const root = mkdtempSync(path.join(os.tmpdir(), 'mods-resolve-'));
  for (const [name, text] of Object.entries(files)) {
    const file = path.join(root, name);
    mkdirSync(path.dirname(file), { recursive: true });
    writeFileSync(file, text);
  }
  return root;
}

test('a file that exists under a mod suffix resolves to itself', () => {
  const root = tree({ 'a.ts': '', 'b.mjs': '' });
  try {
    assert.equal(resolveModFile(path.join(root, 'a.ts')), path.join(root, 'a.ts'));
    assert.equal(resolveModFile(path.join(root, 'b.mjs')), path.join(root, 'b.mjs'));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('an import written without a suffix finds the TypeScript file, .ts before .tsx', () => {
  const root = tree({ 'card.ts': '', 'card.tsx': '', 'view.tsx': '', 'plain.js': '' });
  try {
    assert.equal(resolveModFile(path.join(root, 'card')), path.join(root, 'card.ts'));
    assert.equal(resolveModFile(path.join(root, 'view')), path.join(root, 'view.tsx'));
    assert.equal(resolveModFile(path.join(root, 'plain')), path.join(root, 'plain.js'));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('a .js import with no file behind it names its TypeScript source', () => {
  const root = tree({ 'util.ts': '', 'w.tsx': '', 'm.mts': '', 'real.js': '', 'real.ts': '' });
  try {
    assert.equal(resolveModFile(path.join(root, 'util.js')), path.join(root, 'util.ts'));
    assert.equal(resolveModFile(path.join(root, 'w.jsx')), path.join(root, 'w.tsx'));
    assert.equal(resolveModFile(path.join(root, 'm.mjs')), path.join(root, 'm.mts'));
    assert.equal(resolveModFile(path.join(root, 'real.js')), path.join(root, 'real.js'), 'a .js that exists is itself');
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('a folder import finds its index', () => {
  const root = tree({ 'lib/index.ts': '', 'other/index.js': '' });
  try {
    assert.equal(resolveModFile(path.join(root, 'lib')), path.join(root, 'lib', 'index.ts'));
    assert.equal(resolveModFile(path.join(root, 'other')), path.join(root, 'other', 'index.js'));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('nothing behind an import, or only a non-mod file, resolves to nothing', () => {
  const root = tree({ 'data.json': '{}', 'notes.txt': '' });
  try {
    assert.equal(resolveModFile(path.join(root, 'missing')), null);
    assert.equal(resolveModFile(path.join(root, 'missing.ts')), null);
    assert.equal(resolveModFile(path.join(root, 'data.json')), null);
    assert.equal(resolveModFile(path.join(root, 'notes')), null);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('a hooks module with suffix-less, .js and folder imports loads and runs', async () => {
  const loader = await createLoader({ next: async () => { throw new Error('not a mod?'); } });
  const sealed = await loader.load({
    pluginId: 'imports',
    root: path.join(fixtures, 'imports-mod'),
    entry: 'hooks/register.tsx',
    services: ['mod'],
    seats: ['mods'],
    config: { [MARKER]: { name: 'imports', version: '0.1.0', options: {}, commands: [], tools: [] } },
  });
  const calls = [];
  const ctx = {
    workspaceRoot: '/work',
    async seat(seat, method, params) { calls.push({ method, params }); return {}; },
    async invoke() { return {}; },
  };
  await sealed.scopeHandlers[0](ctx);
  assert.deepEqual(calls.find((call) => call.method === 'ui.status').params, { text: '[INDEX]' });
  await loader.unload('imports');
});

test('an import of a file that is not a mod file is still refused', async () => {
  const root = tree({
    '.claude-plugin/plugin.json': '{ "name": "bad", "version": "0.1.0" }',
    'hooks/hooks.json': '{ "modules": ["./register.ts"] }',
    'hooks/data.json': '{}',
    'hooks/register.ts': "import data from './data.json';\nexport const register = () => {};\n",
  });
  try {
    const loader = await createLoader({ next: async () => { throw new Error('not a mod?'); } });
    await assert.rejects(
      loader.load({
        pluginId: 'bad',
        root,
        entry: 'hooks/register.ts',
        services: ['mod'],
        seats: ['mods'],
        config: { [MARKER]: { name: 'bad', version: '0.1.0', options: {}, commands: [], tools: [] } },
      }),
      /NOT_A_MOD_FILE/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
