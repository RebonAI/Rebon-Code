// `@deepseek-ai/dsh-tools` wrapper module (loop round). The schema layer
// (defineTool + JSON Schema subset) stays the real vendored build; this
// wrapper adds the scheduler-contract constants that live in dsh's
// index.ts (NOT bundled — it is the 86KB ToolRuntime service half we
// deliberately do not vendor; the composition host's tools seat implements
// the same contract, see compose/tools-runtime.js).
//
// Symbol identity: every import of `@deepseek-ai/dsh-tools` resolves here,
// so agent-loop's `ctx.tools[TOOL_RUNTIME_SCHEDULER]` lookup and the seat's
// method installation share this one symbol.
export * from '../../vendor/dsh/tools-schema.js';

/** Scheduler entry point (dsh index.ts:466, transcribed contract). */
export const TOOL_RUNTIME_SCHEDULER = Symbol('@deepseek-ai/dsh-tools.scheduler');

/** Canonical error code for cancellation after a tool body was invoked. */
export const TOOL_ABORTED = 'ABORTED';

/** Canonical error code for cancellation before a tool body was invoked. */
export const TOOL_ABORTED_BEFORE_DISPATCH = 'ABORTED_BEFORE_DISPATCH';
