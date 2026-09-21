// The composition's `ctx.systemPrompt` seat, on the plugin plane.
//
// Ported from `js/compose/systemprompt-runtime.js`. The dsh-facing surface is
// unchanged: plugins call `ctx.systemPrompt.section({name, order, text})` and
// get the exact disposer back, duplicate names throw dsh's message, non-finite
// orders throw dsh's TypeError.
//
// What changed is where the section goes. It used to be
// `callService('system-prompt', 'register', …)` — a synchronous call into a
// kernel living in the same process, compensated by an `unregister` in the
// disposer. Neither survives a process boundary: the call has to be awaited and
// the disposer cannot await. So a section is *reported*: collected here, handed
// to rebon with the rest of the load report, and withdrawn when the plugin
// unloads.
//
// Deliberately NOT provided (v1 honesty boundary, unchanged): provider-function
// `text` (dsh resolves it per assembly with an AssembleContext this side does
// not have — a static composition registers static text), `complete` sections
// (replacing rebon's own prompt belongs to the loop seat, not a side door),
// `context()` / variables / tool-order surfaces. All fail loudly rather than
// silently degrade.
import { Service } from 'cordis';
import { sinkOf } from './registry.mjs';

export default class RebonSystemPromptRuntime extends Service {
  constructor(ctx) {
    super(ctx, 'systemPrompt');
    this.names = new Set();
  }

  /** Register one ordered prompt section; returns the exact disposer. */
  section(entry) {
    const name = entry?.name;
    if (typeof name !== 'string' || name.trim().length === 0) {
      throw new TypeError('systemPrompt.section: section.name must be a non-empty string');
    }
    if (!Number.isFinite(entry.order)) {
      throw new TypeError(`prompt section "${name}" order must be a finite number`);
    }
    if (typeof entry.text === 'function') {
      throw new Error(
        `prompt section "${name}": provider-function text is not supported in the composition (register static text)`,
      );
    }
    if (typeof entry.text !== 'string') {
      throw new TypeError(`prompt section "${name}" text must be a string`);
    }
    if (entry.complete) {
      throw new Error(
        `prompt section "${name}": complete-section replacement is not supported in the composition`,
      );
    }
    if (this.names.has(name)) {
      throw new Error(`prompt section "${name}" is already registered in this scope`);
    }
    const runtime = this;
    // Caller-fork RAII through the Service proxy (llm/tools/web same pattern):
    // the registering plugin's disposal takes the section with it.
    sinkOf(this.ctx)?.section({ name, order: entry.order, text: entry.text });
    return this.ctx.effect(() => {
      runtime.names.add(name);
      return () => {
        runtime.names.delete(name);
      };
    });
  }
}
