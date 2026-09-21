// Who a stray rejection belonged to, and what that costs the entry.
//
// A rejection is unattributed until something says whose it is. An
// `AsyncLocalStorage` store survives timers, microtasks and awaits and the
// `unhandledRejection` handler can read it — measured, not assumed.
//
// The distinction these protect: the store follows the async chain, not the
// clock. A timer started during `apply` that rejects long after the load
// returned still carries its entry. So "whose is it" and "does it fail the
// load" are separate questions, and only the second is about timing.
//
// Run in a child process, because that is the only place this behaviour is
// itself. `node:test` installs its own `unhandledRejection` handler and fails
// the test that produced one, so in-process these would measure the test
// runner rather than the host.
import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

// A `file://` URL, not a path: on Windows an absolute path in an `import` is
// read as a URL whose scheme is the drive letter, and Node refuses it.
const SRC = pathToFileURL(
  path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'src') + path.sep,
).href;

/// Runs `body` in a child with the handler installed, and returns what it said.
function inChild(body) {
  const file = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'rebon-reject-')), 'probe.mjs');
  fs.writeFileSync(file, `
import { asEntryLoad, currentOwner } from '${SRC}ownership.mjs';
import { installRejectionDiagnostic } from '${SRC}cli.mjs';
const said = [];
installRejectionDiagnostic({ write: (text) => { said.push(text.trim()); return true; } });
const settle = () => new Promise((r) => setImmediate(r));
const done = (extra) => { console.log(JSON.stringify({ said, ...extra })); };
${body}
`);
  const out = execFileSync(process.execPath, [file], { encoding: 'utf8', timeout: 20000 });
  return JSON.parse(out.trim().split('\n').pop());
}

test('a rejection raised while an entry is loading fails that load, and says whose', () => {
  const result = inChild(`
let failure = null;
try {
  await asEntryLoad('entry-a', async () => {
    // What a plugin's \`apply\` does when it starts work and gets it wrong.
    Promise.reject(new Error('apply started something that failed'));
    return { loaded: true };
  });
} catch (e) { failure = String(e.message); }
done({ failure });
`);
  assert.match(
    result.failure ?? '',
    /ENTRY_FAILED.*entry-a.*unhandled while loading/s,
    'the load fails, so the composition can skip this entry with a reason',
  );
  assert.match(result.said.join(' | '), /plugin=entry-a during load/);
});

test('a rejection after the load still names the entry, and the load stands', () => {
  const result = inChild(`
const value = await asEntryLoad('entry-b', async () => {
  // Scheduled inside the load and fired well after it returns. Rooted in the
  // chain, late on the clock — which is the whole distinction. Calling it from
  // outside the load instead would create the promise outside the store, and
  // would be testing nothing.
  setTimeout(() => { Promise.reject(new Error('a timer went off later')); }, 30);
  return { loaded: true };
});
await new Promise((r) => setTimeout(r, 120));
done({ value });
`);
  assert.deepEqual(result.value, { loaded: true }, 'the load succeeded and stays succeeded');
  const said = result.said.join(' | ');
  assert.match(said, /plugin=entry-b: /);
  assert.doesNotMatch(said, /during load/);
});

test('a rejection off the chain is unattributed rather than blamed on someone', () => {
  const result = inChild(`
const owner = currentOwner();
Promise.reject(new Error('nobody claims this one'));
await settle();
done({ owner: owner ?? null });
`);
  assert.equal(result.owner, null, 'nothing owns that code');
  const said = result.said.join(' | ');
  assert.match(said, /unattributed: /);
  assert.doesNotMatch(said, /plugin=/, 'a guess would point the next reader at the wrong plugin');
});
