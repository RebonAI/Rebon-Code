// The composition's doorway to rebon, on the plugin plane.
//
// The plane is across a process boundary, so there are no synchronous ops
// into an in-process kernel here. Three consequences follow, and they are the
// protocol's rather than this module's:
//
//   * **Everything is async.** `callService` returned a value from an op; a
//     `seat/call` is a request that has to be answered. The one place this
//     bites is a cordis disposer, which is synchronous and cannot await — see
//     the note on `seat()` below.
//   * **There is no ambient kernel.** A call is only meaningful inside the
//     scope incarnation it was made for, so the doorway exists only inside a
//     handler. `AsyncLocalStorage` is what carries the handler's context down
//     to code that has no way to be handed it — a dsh credential provider is
//     called by an adapter that knows nothing about rebon.
//   * **Registration is not a call.** What a composition provides is
//     reported once, in the `plugin/load` ready report, and revoked by
//     `plugin/unload` draining. It has to be that shape: a compensating
//     `unregister` would have to run from a cordis disposer, and a disposer
//     cannot await the answer.
import { AsyncLocalStorage } from 'node:async_hooks';
import { sessionOf } from './registry.mjs';

const calls = new AsyncLocalStorage();

/// Runs `fn` with `ctx` as the call everything underneath reaches through.
///
/// Every handler the composition serves — service, tool, llm adapter, event
/// delivery — is wrapped in this, so anything it calls can find its way back
/// to rebon without the call context being threaded through dsh code that has
/// no parameter for it.
export function withCall(ctx, fn) {
  return calls.run(ctx, fn);
}

/// The call this code is running inside.
export function currentCall() {
  return calls.getStore();
}

/// The call this code is running inside, or a refusal that says why not.
///
/// The refusal is worth its own code: "cannot read properties of undefined" at
/// the bottom of a dsh stack says nothing, whereas this names the actual rule —
/// rebon is reachable from inside a call, and a disposer or a module top level
/// is not inside one.
function require(what, via) {
  const ctx = calls.getStore();
  if (ctx !== undefined) return ctx;
  // Nothing is being answered, so this is a plugin acting on its own schedule.
  // `via` is the Cordis context of the plugin whose code is running, and the
  // session it is attached to is the honest identity for what it does — the
  // work is still that plugin's, in that session. Without a session there is
  // no identity to use, and inventing one would make the ledger a guess.
  const session = sessionOf(via);
  if (session !== undefined) return session;
  throw new Error(
    `[NO_CALL_CONTEXT] ${what} is not reachable from here: no call rebon made is in flight, `
      + 'and this plugin is not attached to a session. Teardown paths must rely on unload draining instead',
  );
}

/// Calls a kernel seat: the composition's `credentials`, `settings`, `web` and
/// the rest of what rebon installs a plugin to use.
///
/// A request, therefore answerable, therefore **not** usable from a cordis
/// disposer: `dispose()` is synchronous and a cross-process call cannot be.
/// Nothing in the composition needs it to be — the calls that used to happen
/// on the way out were unregistrations, and those are the protocol's job now.
export function seat(name, method, params = null, options = {}) {
  const { via, ...call } = options;
  return require(`seat ${JSON.stringify(name)}`, via).seat(name, method, params, call);
}

/// Invokes one rebon core tool on the session this call belongs to.
///
/// Three gates stand in front of it and only the first is here: the plugin's
/// manifest must declare the tool, rebon's build must expose it, and the
/// embedder's broker decides whether this run is permitted.
export function invokeTool(tool, input = null, options = {}) {
  const { via, ...call } = options;
  return require(`tool ${JSON.stringify(tool)}`, via).invoke(tool, input, call);
}

/// Publishes one event onto rebon's event plane.
///
/// Not the same thing as `emit` below, and the difference is the audience: a
/// chunk is a piece of the answer the caller of *this* call is reading, an
/// event is a fact anyone listening may act on. The composition's runtime
/// events — a dsh tool body appending to the session log — are the second kind.
///
/// Declared as `publishedTopics` in the manifest, separately from what the
/// composition listens to.
export function publish(topic, event = null, options = {}) {
  const { via, ...call } = options;
  return require(`topic ${JSON.stringify(topic)}`, via).publish(topic, event, call);
}

/// Emits one piece of a streamed answer on the call this code is serving.
///
/// This is the transport `op_llm_emit` and `op_tool_serve_emit` used to be,
/// with the pumps gone: a chunk belongs to a call, and the call is the one the
/// handler was entered with rather than an id looked up in a table.
export function emit(piece) {
  return require('emit').emit(piece);
}

/// The workspace root of the session this call belongs to, or undefined
/// outside a call.
export function workspaceRoot() {
  return calls.getStore()?.workspaceRoot;
}

/// The abort signal rebon raises when it asks this call to stop.
export function callSignal() {
  return calls.getStore()?.signal;
}

/// Diagnostics.
///
/// Best-effort on purpose, and never a reason for a call to fail: a log line
/// that cannot be delivered is not worth failing the work that produced it.
/// Inside a call it goes to the kernel's `logger` seat, so the line is
/// attributed to a scope; outside one it goes to stderr, which the supervisor
/// already collects as the host's diagnostic tail. stdout is never touched —
/// that is the protocol's channel.
/// A token bucket per level, so one plugin in a logging loop cannot fill the
/// worker's log.
///
/// Here rather than on rebon's side because the cheapest byte is the one that
/// never crosses the process boundary, and because this side already knows how
/// many it dropped. Drops are reported, at most once a second: replacing a
/// blind spot with a quieter blind spot is not an improvement, and a reader who
/// sees a gap deserves to know it is a gap.
const RATE = { perSecond: 20, burst: 100 };
const buckets = new Map();

function admit(level) {
  const now = Date.now();
  let bucket = buckets.get(level);
  if (bucket === undefined) {
    bucket = { tokens: RATE.burst, last: now, dropped: 0, reportedAt: 0 };
    buckets.set(level, bucket);
  }
  bucket.tokens = Math.min(
    RATE.burst,
    bucket.tokens + ((now - bucket.last) / 1000) * RATE.perSecond,
  );
  bucket.last = now;
  if (bucket.tokens >= 1) {
    bucket.tokens -= 1;
    return { allowed: true, dropped: 0 };
  }
  bucket.dropped += 1;
  if (now - bucket.reportedAt >= 1000) {
    bucket.reportedAt = now;
    const dropped = bucket.dropped;
    bucket.dropped = 0;
    return { allowed: false, dropped };
  }
  return { allowed: false, dropped: 0 };
}

const LEVELS = ['trace', 'debug', 'info', 'warn', 'error'];
export const logger = Object.freeze(Object.fromEntries(LEVELS.map((level) => [
  level,
  (message, options = {}) => {
    const line = String(message);
    // Inside a call, that call is who is speaking. Outside one — a timer, an
    // agent loop driving its own turn — `via` names the plugin, and the
    // session it is attached to is the handle that can still reach the seat.
    //
    // This is the same ladder `require()` climbs, with one difference that
    // matters: it does not throw. A log line is not worth failing a plugin over,
    // and a line that goes to stderr is only ever read when the host dies — the
    // very silence this is fixing.
    const ctx = calls.getStore() ?? sessionOf(options.via);
    if (ctx === undefined) {
      process.stderr.write(`[compose:${level}] ${line}\n`);
      return;
    }
    const { allowed, dropped } = admit(level);
    if (dropped > 0) {
      void ctx
        .seat('logger', 'warn', { message: `${dropped} ${level} line(s) dropped: too many, too fast` })
        .catch(() => {});
    }
    if (!allowed) return;
    void ctx.seat('logger', level, { message: line }).catch(() => {
      process.stderr.write(`[compose:${level}] ${line}\n`);
    });
  },
])));
