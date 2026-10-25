// The plugin host on Verve: what `node cli.mjs --loader <module>` is on Node.
//
// It runs the host's own `runHost` — the same loop, the same framing, the same
// loader seam — over the transport the embedder installs instead of stdio.
// Nothing about the protocol lives here; this module only stands where Node's
// process streams would.
//
// The embedder loads it beside the host's `cli.mjs` and writes that script's own
// address over `HOST_SCRIPT` below, so the import is the host the supervisor
// asked for. It sets `process.argv` the way Node would have for `cli.mjs`'s own
// arguments.
import { diagnostic, installRejectionDiagnostic, resolveLoader, runHost } from 'HOST_SCRIPT';

const transport = globalThis.__rebonVerveTransport;
delete globalThis.__rebonVerveTransport;

// The host reads stdin as an async iterable of byte chunks; `null` is EOF.
const input = {
  async *[Symbol.asyncIterator]() {
    for (;;) {
      const chunk = await transport.read();
      if (chunk === null) return;
      yield chunk;
    }
  },
};

// The host writes frames to a Node `Writable`. The embedder takes every write
// whole and at once — its side is an unbounded queue — so there is no
// backpressure to report: each write is accepted, and its callback runs once
// the bytes are handed over.
const output = {
  write(bytes, done) {
    transport.write(bytes);
    if (typeof done === 'function') queueMicrotask(() => done());
    return true;
  },
  once() { return this; },
  removeListener() { return this; },
};

const error = {
  write(text) {
    transport.diagnostic(String(text));
    return true;
  },
};

installRejectionDiagnostic(error);

let code;
try {
  const flag = process.argv.indexOf('--loader');
  const specifier = flag >= 0 ? process.argv[flag + 1] : process.env.REBON_PLUGIN_LOADER;
  let loader;
  try {
    loader = await resolveLoader(specifier, error);
  } catch (cause) {
    // As on Node: a host that cannot build the loader it was pointed at does
    // not fall back to the built-in one.
    diagnostic(error, cause?.code ?? 'loader_failure', cause?.message ?? 'loader failed');
    code = 2;
  }
  if (code === undefined) {
    code = await runHost({ input, output, error, loader });
  }
} catch (cause) {
  diagnostic(error, cause?.code ?? 'host_failure', cause?.message ?? 'host failed');
  code = 2;
}
transport.exit(code);
