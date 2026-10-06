import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const runtime = path.resolve(here, '..');
const probeScript = path.join(runtime, 'src', 'probe.mjs');
const payload = path.join(runtime, 'payload');

function probe(root, entry, config, { permission = true } = {}) {
  const args = permission
    ? ['--permission', `--allow-fs-read=${path.resolve(runtime, '..')}`, `--allow-fs-read=${root}`, probeScript, root, entry]
    : [probeScript, root, entry];
  if (config !== undefined) args.push(JSON.stringify(config));
  return new Promise((resolve, reject) => {
    execFile(process.execPath, args, { timeout: 30000 }, (error, stdout, stderr) => {
      if (error && !stdout) return reject(new Error(`${error.message}\n${stderr}`));
      resolve(JSON.parse(stdout.trim().split('\n').pop()));
    });
  });
}

test('a package that fits rebon\'s seats reports what it registers', async () => {
  const answer = await probe(payload, 'vendor/dsh/tool-todo.js', { allowParallelInProgress: false });
  assert.equal(answer.ok, true, JSON.stringify(answer));
  assert.deepEqual(answer.tools.map((tool) => tool.name), ['todo_write']);
  assert.deepEqual(answer.inject.required, ['tools']);
});

test('a web search backend registers its provider as a service', async () => {
  const answer = await probe(payload, 'vendor/dsh/web-search-exa.js', {});
  assert.equal(answer.ok, true, JSON.stringify(answer));
  assert.ok(answer.services.some((name) => name.startsWith('web:search')), JSON.stringify(answer.services));
});

test('a package needing a service rebon does not offer names it', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'probe-'));
  writeFileSync(path.join(dir, 'needs.mjs'), "export const name = 'needs';\nexport const inject = ['agents'];\nexport function apply() {}\n");
  const answer = await probe(dir, 'needs.mjs');
  assert.equal(answer.ok, false);
  assert.equal(answer.code, '[MISSING_INJECT]');
  assert.deepEqual(answer.missing, ['agents']);
});

test('a module that is not a Cordis plugin says so', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'probe-'));
  writeFileSync(path.join(dir, 'plain.mjs'), 'export const x = 1;\n');
  const answer = await probe(dir, 'plain.mjs');
  assert.equal(answer.code, '[NOT_CORDIS]');
});

test('the probe runs under the permission model: the package cannot write', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'probe-'));
  const target = path.join(dir, 'escaped.txt');
  writeFileSync(path.join(dir, 'writer.mjs'), `import fs from 'node:fs';\nexport const name = 'writer';\nexport function apply() { fs.writeFileSync(${JSON.stringify(target)}, 'x'); }\n`);
  const answer = await probe(dir, 'writer.mjs');
  assert.equal(answer.ok, false, JSON.stringify(answer));
  assert.match(answer.error, /ERR_ACCESS_DENIED|permission/i);
});

test('the environment shim shows only what the container was granted', async () => {
  const { execFileSync } = await import('node:child_process');
  const script = `import { installModuleResolution } from ${JSON.stringify(new URL('../src/resolve.mjs', import.meta.url).href)};
installModuleResolution();
const { environmentOf } = await import('@deepseek-ai/dsh-environment');
const env = environmentOf({});
console.log(JSON.stringify([env.get('GRANTED_ONE')?.value ?? null, env.get('NOT_GRANTED') ?? null]));`;
  const out = execFileSync(process.execPath, ['--input-type=module', '-e', script], {
    env: { ...process.env, REBON_GRANTED_ENV: 'GRANTED_ONE', GRANTED_ONE: 'yes', NOT_GRANTED: 'secret' },
    encoding: 'utf8',
  });
  assert.deepEqual(JSON.parse(out.trim()), ['yes', null]);
});
