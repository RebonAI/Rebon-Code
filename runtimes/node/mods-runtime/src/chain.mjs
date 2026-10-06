// The hook chain: what `on(event, matcher?, hook)` builds and what a
// dispatch runs through it.
//
// Every hook is `($, e, next)`. `next(e)` runs the hooks beneath and then
// the engine's own behaviour (`core`), resolving to the event's result. A
// hook that returns without calling `next` answers for itself; one that
// calls `next({ ...e, x })` rewrites what the rest sees; one that returns
// nothing after calling `next` leaves the answer from beneath as it is.
//
// A hook that throws or runs out of its budget is skipped and the chain
// continues without it, unless its registration's `.catch` handler answers
// in its place — the rule the mods reference states, and the one that keeps
// a broken mod from blocking a prompt. The failure is reported through
// `onFailure` so the host can write it to the debug log under the mod's name.

const DEFAULT_BUDGET_MS = 15_000;

/// Whether `pattern` (an event name, `*`, `ns.*` or `!name`) selects `event`.
export function patternSelects(pattern, event) {
  if (pattern === '*') return !event.startsWith('telemetry.');
  if (pattern.startsWith('!')) {
    const excluded = pattern.slice(1);
    if (event.startsWith('telemetry.')) return false;
    return !patternSelects(excluded, event);
  }
  if (pattern.endsWith('.*')) return event.startsWith(pattern.slice(0, -1));
  return pattern === event;
}

/// Whether a matcher selects a value: a literal by equality, a RegExp by
/// test, a list by any member, an object by every key, an array value by
/// any item.
export function matcherSelects(matcher, value) {
  if (matcher === undefined) return true;
  if (matcher instanceof RegExp) return typeof value === 'string' && matcher.test(value);
  if (Array.isArray(matcher)) return matcher.some((alternative) => matcherSelects(alternative, value));
  if (Array.isArray(value)) return value.some((item) => matcherSelects(matcher, item));
  if (matcher !== null && typeof matcher === 'object') {
    if (value === null || typeof value !== 'object') return false;
    return Object.keys(matcher).every((key) => matcherSelects(matcher[key], value[key]));
  }
  return matcher === value;
}

/// What `on()` returns: the registration, with `.catch` for a fallback.
class Registration {
  constructor(entry) { this.entry = entry; }
  catch(handler) {
    if (typeof handler !== 'function') throw new TypeError('.catch() takes a function');
    this.entry.catchHandler = handler;
    return this;
  }
}

export class HookChain {
  #entries = [];
  #sequence = 0;
  constructor({ onFailure } = {}) {
    this.onFailure = onFailure ?? (() => {});
  }

  /// `on(pattern, hook)` or `on(pattern, matcher, hook)`.
  on(pattern, matcherOrHook, maybeHook) {
    if (typeof pattern !== 'string' || pattern.length === 0) throw new TypeError('on() needs an event pattern');
    const hook = maybeHook === undefined ? matcherOrHook : maybeHook;
    const matcher = maybeHook === undefined ? undefined : matcherOrHook;
    if (typeof hook !== 'function') throw new TypeError(`on(${JSON.stringify(pattern)}) needs a hook function`);
    if (matcher !== undefined && (matcher === null || typeof matcher !== 'object')) {
      throw new TypeError(`on(${JSON.stringify(pattern)}) takes a matcher object before the hook`);
    }
    const entry = { id: ++this.#sequence, pattern, matcher, hook, catchHandler: undefined };
    this.#entries.push(entry);
    return new Registration(entry);
  }

  /// The patterns registered, in registration order, each once.
  patterns() {
    const seen = new Set();
    const out = [];
    for (const entry of this.#entries) {
      if (seen.has(entry.pattern)) continue;
      seen.add(entry.pattern);
      out.push(entry.pattern);
    }
    return out;
  }

  /// Whether any registration could select `event` (matchers aside).
  listens(event) {
    return this.#entries.some((entry) => patternSelects(entry.pattern, event));
  }

  /// The registrations whose pattern selects `event`. Their matchers are
  /// tested per hook at dispatch, against the event as the hooks above
  /// left it: a rewrite upstream is what a matcher downstream sees.
  hooksFor(event) {
    return this.#entries.filter((entry) => patternSelects(entry.pattern, event));
  }

  /// Runs `event` through the hooks that select it, `core` beneath them.
  ///
  /// `core(e)` is the engine's own behaviour for the event as the chain left
  /// `e`; it is called at most once. The resolved value is the event's result.
  async dispatch(event, input, core, options = {}) {
    const hooks = this.hooksFor(event);
    const budgetMs = options.budgetMs ?? DEFAULT_BUDGET_MS;
    const engine = options.engine;
    const started = Date.now();
    const controller = new AbortController();
    if (options.signal) options.signal.addEventListener('abort', () => controller.abort(), { once: true });
    const remaining = () => Math.max(0, budgetMs - (Date.now() - started));
    const dispatchState = { reachedCore: false, coreInput: undefined };

    const run = async (index, current) => {
      if (index >= hooks.length) {
        dispatchState.reachedCore = true;
        dispatchState.coreInput = current;
        return core(current);
      }
      const entry = hooks[index];
      if (!matcherSelects(entry.matcher, current)) return run(index + 1, current);
      let nextCalled = false;
      let beneath;
      const next = (rewritten = current) => {
        if (nextCalled) return beneath;
        nextCalled = true;
        beneath = run(index + 1, rewritten);
        return beneath;
      };
      next.signal = controller.signal;
      next.budget = () => remaining();
      next.origin = options.origin;
      const frozen = freezeDeep(current);
      try {
        const answer = await withBudget(entry.hook(engine, frozen, next), remaining(), controller);
        if (answer !== undefined) return answer;
        if (nextCalled) return await beneath;
        return run(index + 1, current);
      } catch (error) {
        this.onFailure({ event, pattern: entry.pattern, error });
        if (entry.catchHandler) {
          const caught = Object.assign((rewritten = current) => next(rewritten), next, {
            error,
            cause: error,
          });
          try {
            const answer = await withBudget(entry.catchHandler(engine, frozen, caught), remaining(), controller);
            if (answer !== undefined) return answer;
          } catch (second) {
            this.onFailure({ event, pattern: entry.pattern, error: second, inCatch: true });
          }
        }
        if (nextCalled) return await beneath;
        return run(index + 1, current);
      }
    };
    const result = await run(0, input);
    return { result, reachedCore: dispatchState.reachedCore, coreInput: dispatchState.coreInput };
  }
}

async function withBudget(pending, ms, controller) {
  if (!(pending instanceof Promise)) return pending;
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => {
      controller.abort();
      reject(new Error(`[HOOK_BUDGET] the hook did not answer within ${ms}ms`));
    }, ms);
  });
  try {
    return await Promise.race([pending, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

/// A frozen copy of a plain value, so a hook reads `e` and cannot write it.
export function freezeDeep(value) {
  if (value === null || typeof value !== 'object') return value;
  if (Object.isFrozen(value)) return value;
  if (Array.isArray(value)) return Object.freeze(value.map(freezeDeep));
  const proto = Object.getPrototypeOf(value);
  if (proto !== Object.prototype && proto !== null) return value;
  const out = {};
  for (const key of Object.keys(value)) out[key] = freezeDeep(value[key]);
  return Object.freeze(out);
}
