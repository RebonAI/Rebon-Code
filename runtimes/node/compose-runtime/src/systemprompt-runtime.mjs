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
// Provider-function `text` is resolved once, when rebon asks the entry what it
// registered — after the entry finished loading, so a section that says
// something about the entry's own tools (`ctx.tools.get('read', scope)`) sees
// them. It is resolved with an empty AssembleContext: rebon assembles one
// prompt from what a composition reported, so the global, subject-less
// answer is the one there is. A section whose text resolves empty
// contributes nothing, as in dsh.
//
// Deliberately NOT provided (v1 honesty boundary, unchanged): `complete`
// sections (replacing rebon's own prompt belongs to the loop seat, not a side
// door), `context()` / variables / tool-order surfaces. All fail loudly rather
// than silently degrade.
import { Service } from 'cordis';
import { sinkOf } from './registry.mjs';

/** dsh's centrally allocated section positions (dsh-system-prompt
 *  `SECTION_ORDERS`), so a package placing itself by name lands where it
 *  would in dsh. */
const SECTION_ORDERS = Object.freeze({
  HARNESS_IDENTITY: -1000,
  DEPLOYMENT_PERSONA_PREFIX: 0,
  PLAN_POLICY: 500,
  TEAM_POLICY: 600,
  PTC_ONLY: 800,
  FILE_REFERENCE: 900,
  TOOL_BASH: 1000,
  TOOL_PWSH: 1010,
  TOOL_READ: 1100,
  TOOL_WRITE: 1200,
  TOOL_EDIT: 1300,
  TOOL_GLOB: 1400,
  TOOL_GREP: 1500,
  TOOL_JOBS: 1600,
  TOOL_PTY: 1700,
  TOOL_WEB_SEARCH: 2000,
  TOOL_WEB_FETCH: 2100,
  TOOL_LSP: 2200,
  TOOL_SESSION_QUERY: 2300,
  TOOL_GOAL: 2400,
  TOOL_WORKFLOW: 2600,
  TOOL_RALPH: 2700,
  TOOL_SUBAGENT: 2800,
  TOOL_REPORT: 2900,
  TOOL_COMPUTER_USE: 3000,
  MCP_SERVERS: 3100,
  TOOLS_SDK: 5000,
  DELIVERABLE_FILE_REFERENCES: 9000,
  STRUCTURED_OUTPUT: 9900,
  HARNESS_SOURCE: 10000,
  WEB_SURFACE: 10100,
  DEPLOYMENT_PERSONA_SUFFIX: 10200,
});

/** dsh's runtime-context positions (`CONTEXT_ORDERS`). */
const CONTEXT_ORDERS = Object.freeze({
  WORKING_DIRECTORY: 100,
  SANDBOX_POLICY: 110,
  APPROVAL_POLICY: 115,
  SUBAGENT_DELEGATION: 120,
  TEAM_IDENTITY: 125,
  HARNESS_SOURCE: 200,
  WEB_SURFACE: 210,
});

function placement(table, kind, name) {
  if (!Object.hasOwn(table, name)) {
    throw new TypeError(`unknown prompt ${kind} placement ${JSON.stringify(name)}`);
  }
  return table[name];
}

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
    if (typeof entry.text !== 'string' && typeof entry.text !== 'function') {
      throw new TypeError(`prompt section "${name}" text must be a string or a provider`);
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

  /** dsh `getSectionOrder`: a section's centrally owned position. */
  getSectionOrder(name) {
    return placement(SECTION_ORDERS, 'section', name);
  }

  /** dsh `getContextOrder`: a runtime context's centrally owned position. */
  getContextOrder(name) {
    return placement(CONTEXT_ORDERS, 'context', name);
  }
}
