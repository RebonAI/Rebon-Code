#!/usr/bin/env node
import { pathToFileURL } from 'node:url';
import { NdjsonDecoder, SerializedWriter } from './framing.mjs';
import { PluginHost } from './host.mjs';
import { loadPlugin } from './loader.mjs';
import { currentOwner } from './ownership.mjs';
import { ProtocolError } from './protocol.mjs';

export function diagnostic(stream, code, message) {
  const bounded = String(message).replace(/[\r\n]+/g, ' ').slice(0, 512);
  stream.write(`${JSON.stringify({ level: 'error', code, message: bounded })}\n`);
}
export function installHostConsole(stream) {
  const write = (level, values) => stream.write(`${JSON.stringify({ level, message: values.map(String).join(' ').slice(0, 512) })}\n`);
  return Object.freeze({ log: (...v) => write('info', v), info: (...v) => write('info', v), warn: (...v) => write('warn', v), error: (...v) => write('error', v) });
}

// Frames are not handled one-at-a-time.
//
// A request handler may itself call rebon — that is the whole Node → rebon
// direction — and the answer to that call arrives as a frame on this very
// stream. Awaiting each dispatch before reading the next would mean the handler
// waits for a frame the loop cannot read until the handler returns: a deadlock
// that no amount of care inside a plugin can avoid. So dispatches are tracked
// rather than awaited, and everything that needs them finished waits here.
/// Loads the module that decides what a plugin package *is*.
///
/// The host itself knows one shape: a module exporting `activate`. Anything
/// else — a Cordis plugin mounted into a shared composition realm, say — is a
/// different answer to the same question, and the host has no business knowing
/// which answers exist. So rebon names the module that does, and this loads it.
///
/// The module exports `createLoader({ next })` and returns `{ load, unload }`;
/// `next` is the built-in loader, so an adapter that does not recognise a
/// package hands it back rather than reimplementing it.
export async function resolveLoader(specifier, error) {
  if (!specifier) return {};
  const url = specifier.startsWith('file:') ? specifier : pathToFileURL(specifier).href;
  const module = await import(url);
  if (typeof module.createLoader !== 'function') {
    throw new ProtocolError('loader_invalid', `${specifier} exports no createLoader`);
  }
  const seams = await module.createLoader({ next: loadPlugin });
  if (typeof seams?.load !== 'function') {
    throw new ProtocolError('loader_invalid', `${specifier} produced no load function`);
  }
  return seams;
}

export async function runHost({ input, output, error, loader }) {
  const decoder = new NdjsonDecoder();
  const writer = new SerializedWriter(output);
  const host = new PluginHost(writer, loader ?? {});
  const running = new Set();
  let fatal = null;
  const track = (promise) => {
    const tracked = promise.catch((cause) => { fatal ??= cause; }).finally(() => running.delete(tracked));
    running.add(tracked);
  };
  // Draining rather than a single `Promise.all`: a dispatch that is still
  // running can schedule another one, and the process must not exit under it.
  const quiesce = async () => { while (running.size > 0) await Promise.all([...running]); };
  try {
    for await (const chunk of input) {
      for (const frame of decoder.push(chunk)) {
        if (fatal) throw fatal;
        if (host.shuttingDown) throw new ProtocolError('frame_after_shutdown', 'frame received after shutdown');
        track(host.accept(frame));
      }
      if (fatal) throw fatal;
      if (host.shuttingDown) { await quiesce(); if (fatal) throw fatal; return 0; }
    }
    decoder.finish();
    await quiesce();
    if (fatal) throw fatal;
    diagnostic(error, 'connection_lost', 'stdin reached EOF before successful shutdown');
    return 3;
  } catch (cause) {
    // Terminals already owed still have to reach stdout: a caller waiting on
    // one is not made whole by the host exiting with a diagnosis.
    await quiesce();
    const code = cause?.code ?? 'host_failure';
    diagnostic(error, code, cause?.message ?? 'host failed');
    return code === 'write_error' ? 4 : 2;
  }
}

/// A rejection nobody handled, said out loud instead of swallowed, and
/// attributed where the async chain reaches back to an entry.
///
/// Node does not stop this process for one, and nothing else looks either: a
/// plugin could start an async task, have it fail, and leave no trace anywhere
/// rebon reads.
///
/// A failed promise does not carry who created it, but `plugin/load` runs
/// inside a store that survives timers, microtasks and awaits, so this handler
/// reads the owner straight out of it. What escapes the chain — through an
/// `EventEmitter`, a native callback or a third-party library — is still
/// `unattributed`, which is the truthful answer and is kept distinct from a
/// wrong one.
///
/// It never exits. A stray rejection in one plugin must not take down a host
/// other plugins are being served from; a load still in flight is failed by its
/// own path instead, which is the caller's to run.
export function installRejectionDiagnostic(stream) {
  process.on('unhandledRejection', (reason) => {
    const first = String(reason?.stack ?? reason ?? '').split('\n')[0];
    const owner = currentOwner();
    if (owner === undefined) {
      diagnostic(stream, 'unhandled_rejection', `unattributed: ${first}`);
      return;
    }
    // `loading` says which of the two this is. While a load is in flight the
    // entry did not install correctly and nothing depends on it yet, so the
    // load fails and the composition skips it with a reason. Afterwards the
    // plugin stays ready: a load that already succeeded cannot be un-failed,
    // and tearing down a plane other entries serve from would cost far more
    // than the one bad promise.
    if (owner.loading) {
      owner.rejection = first;
      diagnostic(stream, 'unhandled_rejection', `plugin=${owner.entry} during load: ${first}`);
      return;
    }
    diagnostic(stream, 'unhandled_rejection', `plugin=${owner.entry}: ${first}`);
  });
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  installRejectionDiagnostic(process.stderr);
  const flag = process.argv.indexOf('--loader');
  const specifier = flag >= 0 ? process.argv[flag + 1] : process.env.REBON_PLUGIN_LOADER;
  let loader;
  try {
    loader = await resolveLoader(specifier, process.stderr);
  } catch (cause) {
    // A host that cannot build the loader rebon asked for must not fall back to
    // the built-in one: it would load a different set of plugins than the one
    // rebon believes it asked for, and say nothing.
    diagnostic(process.stderr, cause?.code ?? 'loader_failure', cause?.message ?? 'loader failed');
    process.exitCode = 2;
  }
  if (loader !== undefined) {
    process.exitCode = await runHost({
      input: process.stdin, output: process.stdout, error: process.stderr, loader,
    });
  }
}
