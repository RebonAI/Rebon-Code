// Rebon's hook events, as a mod's native events and as `classic.<Event>`.
//
// Rebon raises the Claude Code settings-hook events (`PreToolUse`,
// `UserPromptSubmit`, `Stop`, ...) through its policy seat; the `mods`
// subscriber hands each one here as `{ event, input }`, `input` being what a
// settings hook would read on stdin. Two things happen with it:
//
// 1. The native event the mods reference maps it to fires through the
//    chain — `tool.call` for PreToolUse, `prompt.submit` for
//    UserPromptSubmit, `turn.complete` for Stop, and so on — and what the
//    chain answered is written back as a settings hook's JSON output, the
//    one vocabulary Rebon's hook runtime projects into effects.
// 2. `classic.<Event>` fires with the stdin shape itself, and a hook's
//    return value is that JSON output, passed through as written.
//
// The answer is a list of such outputs. Rebon parses each one exactly as
// it parses a command hook's stdout, aggregates them and projects the
// effects, so a mod can do precisely what a settings hook can do, and the
// mapping below is the whole statement of what a native answer means.
//
// `tool.call` spans two of Rebon's events. The hook calls `next(e)` for the
// result, and the result arrives with PostToolUse: the chain is suspended at
// its core until then, and the PreToolUse answer is given the moment the
// chain reaches core (the rewritten input) or returns without it (a deny).

const PENDING_CALL_TTL_MS = 10 * 60 * 1000;

const NATIVE_OF = Object.freeze({
  PreToolUse: 'tool.call',
  PostToolUse: 'tool.call',
  PostToolUseFailure: 'tool.call',
  PermissionDenied: 'tool.call',
  PermissionRequest: 'tool.check',
  UserPromptSubmit: 'prompt.submit',
  Stop: 'turn.complete',
  PreCompact: 'session.compact',
  SessionEnd: 'session.end',
});

/// The native event a classic one maps to, when it maps to one.
export function nativeEventOf(classic) {
  return NATIVE_OF[classic];
}

/// The classic events a chain with these patterns would want to hear.
export function classicEventsOf(chain, allEvents) {
  return allEvents.filter((event) => {
    if (chain.listens(`classic.${event}`)) return true;
    const native = NATIVE_OF[event];
    return native !== undefined && chain.listens(native);
  });
}

function plain(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function sameJson(left, right) {
  return JSON.stringify(left) === JSON.stringify(right);
}

/// Runs one classic event through `mod`'s chain and answers the outputs.
export async function dispatchClassic(mod, { event, input }) {
  const outputs = [];
  const native = NATIVE_OF[event];
  if (native !== undefined && mod.chain.listens(native)) {
    const output = await dispatchNative(mod, event, input);
    if (output !== undefined) outputs.push(output);
  }
  const classic = `classic.${event}`;
  if (mod.chain.listens(classic)) {
    const { result, reachedCore } = await mod.chain.dispatch(classic, input, async () => ({}), mod.dispatchOptions());
    if (plain(result) && !(reachedCore && Object.keys(result).length === 0)) outputs.push(result);
  }
  return { outputs };
}

async function dispatchNative(mod, event, input) {
  switch (event) {
    case 'PreToolUse': return preToolUse(mod, input);
    case 'PostToolUse': return settleToolCall(mod, input.tool_use_id, { result: input.tool_response, isError: false }, 'PostToolUse');
    case 'PostToolUseFailure': return settleToolCall(mod, input.tool_use_id, { result: { error: input.error }, isError: true, text: input.error }, 'PostToolUseFailure');
    case 'PermissionDenied': return settleToolCall(mod, input.tool_use_id, { result: { error: input.message ?? 'permission denied' }, isError: true, deny: input.message ?? 'permission denied' }, undefined);
    case 'PermissionRequest': return toolCheck(mod, input);
    case 'UserPromptSubmit': return promptSubmit(mod, input);
    case 'Stop': return turnComplete(mod, input);
    case 'PreCompact': return sessionCompact(mod, input);
    case 'SessionEnd': {
      await mod.chain.dispatch('session.end', { reason: input.reason ?? 'other', resume: {} }, async () => ({ sessionId: input.session_id }), mod.dispatchOptions());
      return undefined;
    }
    default: return undefined;
  }
}

/// PreToolUse → `tool.call`: the chain runs until it reaches core or
/// answers; core parks until the tool's result arrives.
async function preToolUse(mod, input) {
  const e = {
    tool: input.tool_name,
    tool_use_id: input.tool_use_id,
    input: input.tool_input ?? {},
    agentId: input.agent_id,
    origin: { kind: 'model' },
  };
  let settle;
  const parked = new Promise((resolve) => { settle = resolve; });
  let reportPre;
  const preAnswered = new Promise((resolve) => { reportPre = resolve; });
  const core = (final) => {
    reportPre({ continued: true, input: final.input });
    return parked;
  };
  const whole = mod.chain.dispatch('tool.call', e, core, mod.dispatchOptions({ budgetMs: PENDING_CALL_TTL_MS }))
    .then((outcome) => { reportPre({ continued: false, outcome }); return outcome; })
    .catch((error) => { mod.report('tool.call', error); reportPre({ continued: false, outcome: { result: undefined } }); return { result: undefined }; });
  const pre = await preAnswered;
  if (pre.continued) {
    const expiry = setTimeout(() => mod.pendingCalls.delete(input.tool_use_id), PENDING_CALL_TTL_MS);
    mod.pendingCalls.set(input.tool_use_id, { settle, whole, expiry, coreResult: undefined });
    if (!sameJson(pre.input, e.input) && plain(pre.input)) {
      return { hookSpecificOutput: { hookEventName: 'PreToolUse', updatedInput: pre.input } };
    }
    return undefined;
  }
  const answer = pre.outcome?.result;
  if (plain(answer) && typeof answer.deny === 'string') {
    return { hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: answer.deny } };
  }
  if (plain(answer) && 'result' in answer) {
    // A mod answered the call itself. Rebon's hook model has no way to hand
    // the model a result without running the tool, so the call goes on and
    // the mod's answer is reported where the person can read why.
    mod.log(`tool.call for ${e.tool} was answered by the mod; Rebon runs the tool anyway`, 'debug');
  }
  return undefined;
}

/// PostToolUse / PostToolUseFailure / PermissionDenied → the parked
/// `tool.call` core resolves and the hook's final answer is read.
async function settleToolCall(mod, toolUseId, coreResult, eventName) {
  const pending = mod.pendingCalls.get(toolUseId);
  if (!pending) return undefined;
  mod.pendingCalls.delete(toolUseId);
  clearTimeout(pending.expiry);
  pending.settle(coreResult);
  const outcome = await pending.whole;
  const answer = outcome?.result;
  if (eventName === undefined || !plain(answer)) return undefined;
  const output = { hookEventName: eventName };
  let any = false;
  if (eventName === 'PostToolUse' && 'result' in answer && !sameJson(answer.result, coreResult.result)) {
    output.updatedMCPToolOutput = answer.result;
    any = true;
  }
  if (typeof answer.context === 'string' && answer.context.length > 0) {
    output.additionalContext = answer.context;
    any = true;
  }
  return any ? { hookSpecificOutput: output } : undefined;
}

/// PermissionRequest → `tool.check`: `{ decision }` becomes the request's
/// answer; `ask` leaves the prompt to the person.
async function toolCheck(mod, input) {
  const e = { tool: input.tool_name, tool_use_id: input.tool_use_id, input: input.tool_input ?? {}, reason: input.reason };
  const { result } = await mod.chain.dispatch('tool.check', e, async () => ({ decision: 'ask' }), mod.dispatchOptions());
  if (!plain(result)) return undefined;
  if (result.decision === 'allow') {
    return { hookSpecificOutput: { hookEventName: 'PermissionRequest', decision: { behavior: 'allow' } } };
  }
  if (result.decision === 'deny') {
    return { hookSpecificOutput: { hookEventName: 'PermissionRequest', decision: { behavior: 'deny', message: result.reason ?? 'denied by a mod' } } };
  }
  return undefined;
}

/// UserPromptSubmit → `prompt.submit`: a rewrite replaces the prompt, a
/// `drop` refuses it, `context` rides along as additional context.
async function promptSubmit(mod, input) {
  const e = { text: input.prompt ?? '', origin: { kind: 'user' }, sessionId: input.session_id, cwd: input.cwd };
  const { result, reachedCore, coreInput } = await mod.chain.dispatch('prompt.submit', e, async (final) => ({ isQueued: true, text: final.text }), mod.dispatchOptions());
  if (plain(result) && typeof result.drop === 'string') return { decision: 'block', reason: result.drop };
  const specific = { hookEventName: 'UserPromptSubmit' };
  let any = false;
  if (reachedCore && typeof coreInput?.text === 'string' && coreInput.text !== e.text) {
    specific.replacementPrompt = coreInput.text;
    any = true;
  }
  if (plain(result) && typeof result.context === 'string' && result.context.length > 0) {
    specific.additionalContext = result.context;
    any = true;
  }
  if (plain(result) && typeof result.sessionTitle === 'string') {
    specific.sessionTitle = result.sessionTitle;
    any = true;
  }
  return any ? { hookSpecificOutput: specific } : undefined;
}

/// Stop → `turn.complete`: a different `text` shows beneath the answer.
async function turnComplete(mod, input) {
  const e = { reason: input.stop_reason ?? 'end_turn', turnId: input.turn_id ?? null, text: input.last_assistant_message ?? '' };
  const { result } = await mod.chain.dispatch('turn.complete', e, async (final) => ({ text: final.text }), mod.dispatchOptions());
  if (plain(result) && typeof result.text === 'string' && result.text !== e.text && result.text.length > 0) {
    return { systemMessage: result.text };
  }
  return undefined;
}

/// PreCompact → `session.compact`: `{ skip }` keeps the conversation.
async function sessionCompact(mod, input) {
  const e = { trigger: input.trigger ?? 'manual', instructions: input.custom_instructions ?? '', messages: [] };
  const { result } = await mod.chain.dispatch('session.compact', e, async () => ({ messages: [] }), mod.dispatchOptions());
  if (plain(result) && typeof result.skip === 'string') return { decision: 'block', reason: result.skip };
  return undefined;
}
