// Where the vendored JS payload lives.
//
// The payload — Cordis, cosmokit, schemastery and the dsh package set — is this
// package's own, in `payload/`. It used to be owned by `crates/rebon-kernel-js`
// and pointed at from here, because the deno_core host was what shipped and two
// copies of a 600KB vendored tree would diverge. V8 is gone, so the tree moved
// next to its runtime and this file is the only one that changed.
//
// `vendor/cordis/index.js` is the sentinel, for the same reason `bridge.js`
// is on the Rust side: every consumer loads it, so a directory without it is
// not a payload.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SENTINEL = path.join('vendor', 'cordis', 'index.js');
const here = path.dirname(fileURLToPath(import.meta.url));

const holdsPayload = (dir) => fs.existsSync(path.join(dir, SENTINEL));

/// The payload directory, or a refusal naming what was tried.
///
/// A ladder, not a search — the same rule `REBON_PLUGIN_NODE` follows on the
/// Rust side. `REBON_KERNEL_JS_DIR` is an explicit demand: if what it names
/// does not hold a payload, that is the answer, not a reason to go looking
/// somewhere else. Falling back would run the composition against a payload
/// the operator did not choose and never told them.
///
/// Without the override there is one place it can be: `payload/` inside this
/// package, in a checkout and in a deployment alike.
///
/// Throws rather than returning a guess, because every module specifier the
/// composition resolves is built from this path — a wrong one surfaces later
/// as two dozen unrelated "cannot find module" errors.
export function payloadDir() {
  const override = process.env.REBON_KERNEL_JS_DIR;
  if (override) {
    if (holdsPayload(override)) return path.resolve(override);
    throw new Error(
      `[NO_PAYLOAD] REBON_KERNEL_JS_DIR names ${override}, which does not hold ${SENTINEL}`,
    );
  }
  const beside = path.join(here, '..', 'payload');
  if (holdsPayload(beside)) return path.resolve(beside);
  throw new Error(
    `[NO_PAYLOAD] no vendored JS payload found (looked for ${SENTINEL} under ${beside}); `
      + 'set REBON_KERNEL_JS_DIR to the directory holding it',
  );
}
