// The composition's `ctx.subprocess` seat: dsh's subprocess seam, with rebon
// starting the program.
//
// DeepSeek Harness's shell executors (`dsh-bash-local`, `dsh-pwsh-local`)
// run commands through `ctx.subprocess.spawn(spec)`. dsh's own backend,
// `dsh-subprocess-local`, is `child_process` plus native addons (node-pty,
// and koffi for Windows job objects), none of which a plugin on Verve has.
// So the realm answers the seam itself, and the program is started by rebon
// through the `process` seat (`crates/rebon-plugin-host/src/process_seat.rs`):
// on the shared host only, inside a composition tool call the person
// allowed.
//
// The contract kept is dsh's (`@deepseek-ai/dsh-subprocess`): `spawn`
// returns a live handle synchronously; `done` settles with
// `{ exitCode, signal }`; collected streams are read by offset with
// `readFrom`, keep their tail within `maxBytes`, and say when a read was
// lossy; `terminate` (and the spec's abort signal) ends the program;
// `waitForExit` waits for it. What is not offered is refused by name rather
// than half-done: raw `pipe`/`inherit` streams, a control channel, spill
// files and terminals.
import { Service } from 'cordis';
import { seat } from './bridge.mjs';

class SubprocessError extends Error {
  constructor(code, message) {
    super(`${code} ${message}`);
    this.code = code;
    this.name = 'SubprocessError';
  }
}

/// How long one `wait` holds on rebon's side before answering with nothing.
const WAIT_SLICE_MS = 10_000;

/// One collect-mode stream: the retained tail, by whole-stream byte offset.
class Collected {
  constructor(maxBytes) {
    this.maxBytes = Number.isFinite(maxBytes) && maxBytes > 0 ? maxBytes : Infinity;
    this.bytes = Buffer.alloc(0);
    /** Whole-stream offset of `bytes[0]`. */
    this.start = 0;
  }

  push(text, dropped = 0) {
    this.start += dropped;
    if (text.length === 0) return;
    this.bytes = Buffer.concat([this.bytes, Buffer.from(text, 'utf8')]);
    if (this.bytes.length > this.maxBytes) {
      const excess = this.bytes.length - this.maxBytes;
      this.bytes = this.bytes.subarray(excess);
      this.start += excess;
    }
  }

  /// dsh's `SubprocessOutputReader.readFrom`.
  readFrom(fromByte) {
    const end = this.start + this.bytes.length;
    if (fromByte < this.start) {
      return { text: this.bytes.toString('utf8'), nextOffset: end, lossy: true };
    }
    const at = Math.min(fromByte, end) - this.start;
    return { text: this.bytes.subarray(at).toString('utf8'), nextOffset: end, lossy: false };
  }
}

function collectMode(mode, stream) {
  if (mode !== null && typeof mode === 'object' && Number.isFinite(mode.maxBytes)) return mode;
  throw new SubprocessError(
    '[UNSUPPORTED]',
    `${stream} ${JSON.stringify(mode)} is not offered: rebon's subprocess seam collects output (\`{ maxBytes }\`)`,
  );
}

export default class RebonSubprocess extends Service {
  constructor(ctx) {
    super(ctx, 'subprocess');
  }

  /// dsh `SubprocessRuntime.spawn`.
  spawn(spec) {
    const argv = spec?.argv;
    if (!Array.isArray(argv) || argv.length === 0 || !argv.every((part) => typeof part === 'string')) {
      throw new TypeError('subprocess.spawn needs a non-empty argv of strings');
    }
    const stdio = spec.stdio ?? {};
    if (stdio.control !== undefined) {
      throw new SubprocessError('[UNSUPPORTED]', 'a control channel is not offered by rebon\'s subprocess seam');
    }
    const stdin = stdio.stdin;
    if (stdin !== 'ignore' && !(stdin !== null && typeof stdin === 'object' && typeof stdin.data === 'string')) {
      throw new SubprocessError('[UNSUPPORTED]', `stdin ${JSON.stringify(stdin)} is not offered: use 'ignore' or { data }`);
    }
    const stdout = new Collected(collectMode(stdio.stdout, 'stdout').maxBytes);
    const stderr = new Collected(collectMode(stdio.stderr, 'stderr').maxBytes);
    // `undefined` is dsh's tombstone for an ambient entry; JSON says null.
    const env = Object.fromEntries(Object.entries(spec.env ?? {}).map(([name, value]) => [name, value ?? null]));
    const via = this.ctx;
    let id;
    let terminated = false;
    let exited = false;
    const exitWaiters = new Set();
    const kill = () => {
      if (id === undefined || exited) return;
      void seat('process', 'kill', { id }, { via }).catch(() => {});
    };
    const done = (async () => {
      const started = await seat('process', 'spawn', {
        argv: [...argv],
        ...(typeof spec.cwd === 'string' ? { cwd: spec.cwd } : {}),
        env,
        ...(typeof stdin === 'object' ? { stdin: stdin.data } : {}),
      }, { via });
      id = started.id;
      if (terminated) kill();
      for (;;) {
        const answer = await seat('process', 'wait', { id, timeoutMs: WAIT_SLICE_MS }, { via });
        stdout.push(answer.stdout?.text ?? '', answer.stdout?.dropped ?? 0);
        stderr.push(answer.stderr?.text ?? '', answer.stderr?.dropped ?? 0);
        if (answer.exit !== null && answer.exit !== undefined) {
          return { exitCode: answer.exit.exitCode ?? null, signal: answer.exit.signal ?? null };
        }
      }
    })().finally(() => {
      exited = true;
      for (const resolve of exitWaiters) resolve(true);
      exitWaiters.clear();
    });
    const terminate = () => {
      terminated = true;
      kill();
    };
    if (spec.signal?.aborted) terminate();
    spec.signal?.addEventListener?.('abort', terminate, { once: true });
    return {
      stdin: undefined,
      stdout: undefined,
      stderr: undefined,
      control: undefined,
      collected: { stdout, stderr },
      done,
      terminate,
      waitForExit(signal) {
        if (exited) return Promise.resolve(true);
        return new Promise((resolve) => {
          exitWaiters.add(resolve);
          signal?.addEventListener?.('abort', () => {
            exitWaiters.delete(resolve);
            resolve(false);
          }, { once: true });
        });
      },
    };
  }

  /// dsh's terminal primitive, which rebon does not offer.
  spawnTerminal() {
    throw new SubprocessError('[UNSUPPORTED]', 'terminals are not offered by rebon\'s subprocess seam');
  }
}
