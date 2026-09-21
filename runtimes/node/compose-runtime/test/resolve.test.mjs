// The composition's module resolution.
//
// Every specifier the composition can name has to actually load on real Node —
// the premise of this file, and the only one: there is one map, in
// `resolve.mjs`. There used to be a second in the deno_core host, and a test
// here that kept the two spellings of that one fact in step. The host is gone,
// so the comparison has nothing left to compare against.
import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { payloadDir } from '../src/payload.mjs';
import { installModuleResolution, localModules, payloadModules } from '../src/resolve.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));

test('every vendored specifier imports on real Node', async () => {
  installModuleResolution();
  const specifiers = [...payloadModules(payloadDir()).keys(), ...localModules().keys()];
  const failures = [];
  for (const specifier of specifiers) {
    try {
      const module = await import(specifier);
      assert.ok(Object.keys(module).length > 0, `${specifier} exports nothing`);
    } catch (error) {
      failures.push(`${specifier}: ${String(error?.message ?? error).split('\n')[0]}`);
    }
  }
  assert.deepEqual(failures, []);
});

test('a payload directory without the sentinel is refused by name', () => {
  const before = process.env.REBON_KERNEL_JS_DIR;
  process.env.REBON_KERNEL_JS_DIR = path.join(here, 'no-such-payload');
  try {
    assert.throws(() => payloadDir(), /\[NO_PAYLOAD\]/);
  } finally {
    if (before === undefined) delete process.env.REBON_KERNEL_JS_DIR;
    else process.env.REBON_KERNEL_JS_DIR = before;
  }
});
