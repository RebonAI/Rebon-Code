// The composition's `ctx.tools` seat, on the plugin plane.
//
// Ported from `js/compose/tools-runtime.js`. The dsh-facing surface is
// unchanged: plugins call `ctx.tools.register(defineTool({...}))` and get the
// exact disposer back, definitions arrive with `parameters` already compiled to
// JSON Schema and argument validation baked into `execute`, and the loop drives
// tools through the symbol-keyed scheduler contract.
//
// What changed:
//
//   * **The serve pump is gone.** `registerServeTarget(name, …)` and
//     `op_tool_serve_next` are replaced by the tool being a *declared* tool of
//     the loading plugin, called back through `tool/call`. A tool is offered to
//     a model, so the registration carries the description and input schema
//     rather than a name alone — which is what the plane needs anyway.
//   * **`callService('tool-registry', 'register', …)` is gone.** The tool is
//     reported by `plugin/load` and withdrawn by `plugin/unload` draining, so
//     the compensating `unregister` a disposer used to make — and could not
//     have made across a process boundary — has nothing left to compensate.
//     Shadowing a builtin name is rebon's judgement to make and rebon's to log;
//     the composition no longer hears about it, which is why the warning that
//     used to be printed here is not.
//   * **`exec.agent.session.append` publishes.** It was a kernel event emitted
//     from inside the isolate; it is `event/emit` now, declared as
//     `publishedTopics`, and it is the reason a tool-providing plugin declares
//     `compose:session/append`.
import { Service } from 'cordis';
import { TOOL_RUNTIME_SCHEDULER } from '@deepseek-ai/dsh-tools';
import { invokeTool, publish, withCall } from './bridge.mjs';
import { sinkOf } from './registry.mjs';

/** The topic a tool body's session writes land on. */
export const SESSION_APPEND_TOPIC = 'compose:session/append';

/** Extract a `[BRACKET_CODE]` prefix from a seat error message, if any. */
function bracketCode(message) {
  const match = /^\[([A-Z_]+)\]/.exec(message);
  return match ? match[1] : undefined;
}

/** Per-run context/conclusion collection installed by scheduler.prepare. */
const runStates = new WeakMap();

export default class RebonToolsRuntime extends Service {
  constructor(ctx) {
    super(ctx, 'tools');
    this.names = new Set();
    // Local definition table for the scheduler's in-composition dispatch
    // (register() below keeps it in step with what rebon was told).
    this.defs = new Map();
  }

  /**
   * dsh scheduler contract.
   *
   * agent-loop's tool-calls scheduler drives tools through this symbol-keyed
   * view (dsh's private loop↔tools seam, contract transcribed from
   * packages/core/tools/src/index.ts:451). Dispatch resolution: a
   * composition-registered definition runs here; anything else goes to rebon's
   * own tools through `tool/invoke`, whose refusals (stable bracketed codes)
   * come back as error RESULTS for the model, not crashes. v1 honesty
   * boundary: no pre/post-execute pipeline — prepare always dispatches,
   * finalize/finish pass through.
   *
   * A getter rather than a field on purpose: read through the Cordis Service
   * proxy, `this.ctx` is the *caller's* context — the loop plugin driving the
   * turn. That is the identity a `tool/invoke` made on the loop's own schedule
   * has to carry, and capturing it here is the only place it is still known.
   */
  get [TOOL_RUNTIME_SCHEDULER]() {
    const runtime = this;
    const via = this.ctx;
    return {
      async prepare(exec) {
        const state = { contexts: [], concluded: false };
        const run = {
          ...exec,
          rootCallId: exec.rootCallId ?? exec.callId,
          deferContext(message) {
            state.contexts.push(message);
          },
          concludeTurn() {
            state.concluded = true;
          },
        };
        runStates.set(run, state);
        return { kind: 'dispatch', exec: run };
      },
      dispatch: (exec) => runtime._dispatch(exec, via),
      async finalize(_exec, result) {
        return result;
      },
      finish(_exec, result) {
        return result;
      },
    };
  }

  async _dispatch(exec, via) {
    const state = runStates.get(exec) ?? { contexts: [], concluded: false };
    try {
      const def = this.defs.get(exec.name);
      let content;
      if (def !== undefined) {
        const value = await def.execute(exec.arguments, exec);
        content = def.output.render(exec.arguments, value);
      } else {
        const value = await invokeTool(exec.name, exec.arguments ?? {}, { via });
        content = [{
          type: 'text',
          text: typeof value === 'string' ? value : JSON.stringify(value),
        }];
      }
      return {
        kind: 'final-result',
        result: {
          content,
          isError: false,
          ...(state.contexts.length > 0 ? { additionalContexts: state.contexts } : {}),
          ...(state.concluded ? { concludesTurn: true } : {}),
        },
      };
    } catch (error) {
      const message = String(error?.message ?? error);
      return {
        kind: 'final-result',
        result: {
          content: [{ type: 'text', text: `Error: ${message}` }],
          isError: true,
          error: {
            message,
            info: {
              name: error?.name ?? 'Error',
              code: error?.code ?? bracketCode(message) ?? 'TOOL_FAILED',
            },
          },
        },
      };
    }
  }

  /**
   * dsh concurrency classification. v1: everything is exclusive — strictly
   * serial scheduling is always semantically correct, and it sidesteps the
   * parallel pool until `isConcurrencySafe` mapping is wired.
   */
  executionMode(_exec) {
    return { kind: 'exclusive' };
  }

  /**
   * Register one dsh-shaped tool definition. Mirrors dsh
   * `ToolRuntime.register` validation; returns the exact disposer.
   */
  register(definition) {
    const name = definition?.name;
    if (typeof name !== 'string' || name.trim().length === 0) {
      throw new TypeError('tools.register: definition.name must be a non-empty string');
    }
    const output = definition.output;
    if (
      output === undefined || typeof output !== 'object'
      || typeof output.render !== 'function'
    ) {
      throw new TypeError(`tool "${name}" must declare output { schema, render }`);
    }
    if (typeof definition.execute !== 'function') {
      throw new TypeError(`tool "${name}" must declare execute()`);
    }
    const timeoutMs = definition.timeoutMs;
    if (timeoutMs !== undefined && (!Number.isFinite(timeoutMs) || timeoutMs <= 0)) {
      throw new TypeError(`tool "${name}" timeoutMs must be a positive finite number`);
    }
    if (this.names.has(name)) {
      throw new Error(`tool "${name}" is already registered in this scope`);
    }
    const runtime = this;
    // `this.ctx` is the CALLER's context through the Service proxy, so the
    // registration is attributed to the plugin that made it and the effect
    // unwinds when that plugin disposes — Cordis RAII, llm-runtime same
    // pattern. Read synchronously: which plugin is registering is a fact
    // about now, and an await would let another one interleave.
    const sink = sinkOf(this.ctx);
    sink?.tool(
      {
        name,
        description: String(definition.description ?? ''),
        inputSchema: definition.parameters ?? { type: 'object' },
      },
      (input, callCtx) => withCall(callCtx, () => runtime._execute(name, input, callCtx)),
    );
    return this.ctx.effect(() => {
      runtime.names.add(name);
      runtime.defs.set(name, definition);
      return () => {
        runtime.names.delete(name);
        runtime.defs.delete(name);
      };
    });
  }

  /// Runs one registered tool for a `tool/call`.
  ///
  /// Resolved through the live table rather than closing over the definition:
  /// a tool whose plugin disposed it is gone, and answering with a body that
  /// is no longer registered would make disposal a lie.
  async _execute(name, input, callCtx) {
    const definition = this.defs.get(name);
    if (definition === undefined) {
      const error = new Error(`tool "${name}" is no longer registered in this composition`);
      error.code = '[STALE_PROVIDER]';
      throw error;
    }
    const callId = `compose:${callCtx.scopeId ?? 'scope'}:${name}`;
    const exec = {
      callId,
      rootCallId: callId,
      name,
      arguments: input,
      signal: callCtx.signal,
      // dsh tools write per-session facts through `exec.agent.session.append`.
      // On the plane that is a published event: the tool is telling rebon
      // something happened, and rebon decides who hears it.
      agent: {
        session: {
          append(type, data) {
            void publish(SESSION_APPEND_TOPIC, { type, data, tool: name, callId })
              .catch(() => {});
          },
        },
      },
    };
    // defineTool's execute validates arguments first (ToolArgsError with
    // INVALID_ARGS semantics flows out as this call's terminal).
    const value = await definition.execute(input, exec);
    const content = definition.output.render(input, value);
    return { content, isError: false };
  }
}
