// packages/core/session/src/index.ts
import { Service } from "@deepseek-ai/cordis";
import { isAbsolute } from "node:path";
import { deepFreeze } from "@deepseek-ai/dsh-llm";
import { scopeOf, scopeTarget } from "@deepseek-ai/dsh-scope";

// packages/core/session/src/types.ts
function SessionId(id) {
  return id;
}
var SESSION_FORMAT_VERSION = 0;

// packages/core/session/src/json.ts
function hasIntrinsicConstructor(prototype, name) {
  const descriptor = Object.getOwnPropertyDescriptor(prototype, "constructor");
  const constructor = descriptor?.value;
  if (typeof constructor !== "function") return false;
  try {
    return constructor.name === name && constructor.prototype === prototype && Function.prototype.toString.call(constructor) === `function ${name}() { [native code] }`;
  } catch {
    return false;
  }
}
function isIntrinsicObjectPrototype(value) {
  return Object.getPrototypeOf(value) === null && hasIntrinsicConstructor(value, "Object");
}
function hasPlainArrayPrototype(value) {
  const prototype = Object.getPrototypeOf(value);
  if (!Array.isArray(prototype) || !hasIntrinsicConstructor(prototype, "Array")) return false;
  const objectPrototype = Object.getPrototypeOf(prototype);
  return typeof objectPrototype === "object" && objectPrototype !== null && isIntrinsicObjectPrototype(objectPrototype);
}
function hasPlainObjectPrototype(value) {
  const prototype = Object.getPrototypeOf(value);
  return prototype === null || typeof prototype === "object" && isIntrinsicObjectPrototype(prototype);
}
function enumerableStringKeys(value) {
  const keys = Reflect.ownKeys(value);
  if (keys.some((key) => typeof key !== "string" || !Object.prototype.propertyIsEnumerable.call(value, key))) return void 0;
  return keys;
}
function walkJsonValue(value, detach) {
  const ancestors = /* @__PURE__ */ new Set();
  let root;
  const assign = (destination, item) => {
    if (destination === void 0) return;
    if (destination.kind === "root") {
      root = item;
    } else if (destination.kind === "array") {
      destination.target[destination.index] = item;
    } else {
      Object.defineProperty(destination.target, destination.key, {
        value: item,
        enumerable: true,
        configurable: true,
        writable: true
      });
    }
  };
  const tasks = [{
    kind: "visit",
    value,
    ...detach ? { destination: { kind: "root" } } : {}
  }];
  for (let task = tasks.pop(); task !== void 0; task = tasks.pop()) {
    if (task.kind === "leave") {
      ancestors.delete(task.source);
      continue;
    }
    if (task.kind === "array-item") {
      if (!Object.prototype.hasOwnProperty.call(task.source, task.index)) return void 0;
      tasks.push({
        kind: "visit",
        value: task.source[task.index],
        ...task.target === void 0 ? {} : { destination: { kind: "array", target: task.target, index: task.index } }
      });
      continue;
    }
    if (task.kind === "object-property") {
      tasks.push({
        kind: "visit",
        value: task.source[task.key],
        ...task.target === void 0 ? {} : { destination: { kind: "object", target: task.target, key: task.key } }
      });
      continue;
    }
    const current = task.value;
    if (current === null) {
      assign(task.destination, null);
      continue;
    }
    if (typeof current === "boolean" || typeof current === "string") {
      assign(task.destination, current);
      continue;
    }
    if (typeof current === "number") {
      if (!Number.isFinite(current) || Object.is(current, -0)) return void 0;
      assign(task.destination, current);
      continue;
    }
    if (typeof current !== "object") return void 0;
    if (ancestors.has(current)) return void 0;
    if (Array.isArray(current)) {
      if (!hasPlainArrayPrototype(current)) return void 0;
      const length = current.length;
      if (Reflect.ownKeys(current).length !== length + 1) return void 0;
      const target2 = detach ? [] : void 0;
      if (target2 !== void 0) assign(task.destination, target2);
      ancestors.add(current);
      tasks.push({ kind: "leave", source: current });
      for (let index = length - 1; index >= 0; index--) {
        tasks.push({ kind: "array-item", source: current, index, ...target2 === void 0 ? {} : { target: target2 } });
      }
      continue;
    }
    if (!hasPlainObjectPrototype(current)) return void 0;
    const keys = enumerableStringKeys(current);
    if (keys === void 0) return void 0;
    const target = detach ? {} : void 0;
    if (target !== void 0) assign(task.destination, target);
    ancestors.add(current);
    tasks.push({ kind: "leave", source: current });
    for (let index = keys.length - 1; index >= 0; index--) {
      const key = keys[index];
      if (key === void 0) return void 0;
      tasks.push({ kind: "object-property", source: current, key, ...target === void 0 ? {} : { target } });
    }
  }
  return detach ? root : true;
}
function snapshotJsonValue(value) {
  return walkJsonValue(value, true);
}
function isJsonValue(value) {
  return walkJsonValue(value, false) === true;
}

// packages/core/session/src/surface.ts
var SURFACE_EVENT_TYPES = /* @__PURE__ */ new Set([
  "user/message",
  "assistant/message",
  "tool/result"
]);
function isSurfaceEligibleType(type) {
  return SURFACE_EVENT_TYPES.has(type);
}
function isSurfaceEvent(event) {
  if (!SURFACE_EVENT_TYPES.has(event.type)) return false;
  return event.surfaceOp !== void 0;
}
function isAppendSurfaceEvent(event) {
  return isSurfaceEvent(event) && event.surfaceOp === "append";
}
function isReplacementSurfaceEvent(event) {
  return isSurfaceEvent(event) && event.surfaceOp !== "append";
}
function deriveEventMessage(event) {
  switch (event.type) {
    // Ordinary prompts and injected context project in user role: the event's
    // model-facing content stays verbatim. Do NOT re-add per-type framing
    // (e.g. `<context>`) here: framing is caller-owned — a producer bakes it
    // into `content`, as agent-instructions does with `<system-reminder>` — or,
    // if reintroduced, must be driven by the event `meta` map and a dedicated
    // renderer, keeping this projection a verbatim pass-through. See the
    // deferred design note in
    // ../../../../.agents/notes/implemented/simplification/2026-07-20-unwrap-injected-content-envelopes.md
    case "user/message": {
      return event.data;
    }
    case "assistant/message": {
      if (event.data.message.content.length === 0) return null;
      return event.data.message;
    }
    case "tool/result": {
      return event.data.message;
    }
    default:
      return null;
  }
}
function createFoldState() {
  return { nodes: [], replaceGeneration: 0 };
}
function isEventSeq(value) {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}
function isReplaceOp(value) {
  const op = value;
  return Object.keys(op).length === 3 && Object.hasOwn(op, "op") && Object.hasOwn(op, "start") && Object.hasOwn(op, "end") && op["op"] === "replace" && isEventSeq(op["start"]) && isEventSeq(op["end"]);
}
function surfaceOpOf(event) {
  const raw = event;
  if (!isSurfaceEligibleType(event.type)) {
    if (raw.surfaceOp !== void 0) {
      throw new Error(`session event "${event.type}" is not surface-eligible and cannot carry surfaceOp`);
    }
    if (raw.sourceEventSeqs !== void 0) {
      throw new Error(`session event "${event.type}" is not surface-eligible and cannot carry sourceEventSeqs`);
    }
    return;
  }
  const op = raw.surfaceOp;
  if (op === void 0) {
    throw new Error(`session event "${event.type}" is surface-eligible and requires a surfaceOp marker`);
  }
  if (op === "append") return op;
  if (op === null || typeof op !== "object" || Array.isArray(op)) {
    throw new Error(`session event "${event.type}" carries an invalid surfaceOp`);
  }
  if (!isReplaceOp(op)) {
    throw new Error(`session event "${event.type}" carries an invalid replace surfaceOp`);
  }
  return op;
}
function assertProvenance(event, shadowedSeqs) {
  const raw = event.sourceEventSeqs;
  const sources = /* @__PURE__ */ new Set();
  if (raw !== void 0) {
    if (!Array.isArray(raw)) {
      throw new Error(`sourceEventSeqs on event at seq ${event.seq} must be an array when present`);
    }
    if (raw.length === 0 && event.type !== "assistant/message") {
      throw new Error("sourceEventSeqs must not be empty except on assistant/message");
    }
    let nonEarlierSource;
    for (const source of raw) {
      if (!isEventSeq(source)) {
        throw new Error(`session event "${event.type}" sourceEventSeqs must densely contain non-negative safe integers`);
      }
      sources.add(source);
      if (nonEarlierSource === void 0 && source >= event.seq) nonEarlierSource = source;
    }
    if (sources.size !== raw.length) {
      throw new Error("sourceEventSeqs must not contain duplicates");
    }
    if (nonEarlierSource !== void 0) {
      throw new Error(`sourceEventSeqs must reference earlier events: ${nonEarlierSource} >= current seq ${event.seq}`);
    }
  }
  const missing = shadowedSeqs.filter((seq) => !sources.has(seq));
  if (missing.length > 0) {
    throw new Error(`surface replace: sourceEventSeqs must include every shadowed surface node; missing ${missing.join(", ")}`);
  }
}
function replacementRange(state, op) {
  const startIdx = state.nodes.indexOf(op.start);
  if (startIdx === -1) {
    throw new Error(`surface replace: start seq ${op.start} not found in surface`);
  }
  const endIdx = state.nodes.indexOf(op.end);
  if (endIdx === -1) {
    throw new Error(`surface replace: end seq ${op.end} not found in surface`);
  }
  if (startIdx > endIdx) {
    throw new Error(`surface replace: start seq ${op.start} (index ${startIdx}) is after end seq ${op.end} (index ${endIdx})`);
  }
  return {
    startIdx,
    endIdx,
    shadowedSeqs: state.nodes.slice(startIdx, endIdx + 1)
  };
}
function isDeepEqualJson(a, b) {
  if (a === b) return true;
  if (Array.isArray(a) || Array.isArray(b)) {
    if (!Array.isArray(a) || !Array.isArray(b) || a.length !== b.length) return false;
    return a.every((item, i) => isDeepEqualJson(item, b[i]));
  }
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  const aKeys = Object.keys(a);
  const bRecord = b;
  if (aKeys.length !== Object.keys(b).length) return false;
  return aKeys.every((key) => Object.hasOwn(b, key) && isDeepEqualJson(a[key], bRecord[key]));
}
function assertToolResultRewrite(event, shadowedSeqs, events, baseSeq) {
  if (event.type !== "tool/result") return;
  if (shadowedSeqs.length !== 1) {
    throw new Error("tool/result surface replacement must rewrite exactly one current node");
  }
  for (const originalSeq of shadowedSeqs) {
    const original = events[originalSeq - baseSeq];
    if (original?.type !== "tool/result") {
      throw new Error("tool/result surface replacement must target a current tool/result");
    }
    const originalRest = { ...original.data };
    const replacementRest = { ...event.data };
    const originalResult = original.data.message.content[0];
    const replacementResult = event.data.message.content[0];
    originalRest["message"] = {
      ...original.data.message,
      content: [{ ...originalResult, content: null }]
    };
    replacementRest["message"] = {
      ...event.data.message,
      content: [{ ...replacementResult, content: null }]
    };
    if (!isDeepEqualJson(originalRest, replacementRest)) {
      throw new Error("tool/result surface replacement may change only content");
    }
  }
}
function planSurfaceEvent(state, event, expectedSeq, events, baseSeq) {
  if (event.seq !== expectedSeq) {
    throw new Error(`session event seq ${event.seq} is not contiguous; expected ${expectedSeq}`);
  }
  const surfaceOp = surfaceOpOf(event);
  if (surfaceOp === void 0) return;
  if (surfaceOp === "append") {
    assertProvenance(event, []);
    return { kind: "append", seq: event.seq };
  }
  const range = replacementRange(state, surfaceOp);
  assertProvenance(event, range.shadowedSeqs);
  assertToolResultRewrite(event, range.shadowedSeqs, events, baseSeq);
  return {
    kind: "replace",
    seq: event.seq,
    start: surfaceOp.start,
    end: surfaceOp.end,
    ...range
  };
}
function applySurfaceEvent(state, event, expectedSeq, events, baseSeq) {
  const plan = planSurfaceEvent(state, event, expectedSeq, events, baseSeq);
  return applySurfacePlan(state, plan);
}
function applySurfacePlan(state, plan) {
  if (plan?.kind === "append") {
    state.nodes.push(plan.seq);
  } else if (plan?.kind === "replace") {
    state.nodes.splice(plan.startIdx, plan.endIdx - plan.startIdx + 1, plan.seq);
    state.replaceGeneration += 1;
  }
  if (plan?.kind !== "replace") return;
  return {
    seq: plan.seq,
    start: plan.start,
    end: plan.end,
    shadowedSeqs: plan.shadowedSeqs
  };
}
function foldSurface(events) {
  const state = createFoldState();
  const replacements = [];
  for (const [index, event] of events.entries()) {
    const replacement = applySurfaceEvent(state, event, index, events, 0);
    if (replacement !== void 0) replacements.push(replacement);
  }
  return { nodes: [...state.nodes], replacements };
}
var SurfaceManager = class {
  /**
   * @param log - Contiguous complete log or loaded event window.
   * @param baseSeq - Absolute sequence of the window's first event.
   */
  constructor(log, baseSeq = 0) {
    this.log = log;
    this.baseSeq = baseSeq;
    this._lastProcessedSeq = baseSeq - 1;
  }
  log;
  baseSeq;
  /** Shared transition state; replacement history is not retained. */
  _state = createFoldState();
  /** Last processed absolute seq. */
  _lastProcessedSeq;
  /** Candidate already validated by `validateNext`, pending exact log admission. */
  _pendingPlan;
  /**
   * Validate the next candidate without mutating the committed surface.
   * @param event - candidate event that has not entered the log yet.
   */
  validateNext(event) {
    if (this._lastProcessedSeq < this.baseSeq + this.log.length - 1) this._processDelta();
    const expectedSeq = this.baseSeq + this.log.length;
    this._pendingPlan = {
      event,
      expectedSeq,
      plan: planSurfaceEvent(this._state, event, expectedSeq, this.log, this.baseSeq)
    };
  }
  /** Monotonic count of folded positional replacements. */
  get replaceGeneration() {
    if (this._lastProcessedSeq < this.baseSeq + this.log.length - 1) this._processDelta();
    return this._state.replaceGeneration;
  }
  /** Surface event sequences in model-visible order. */
  get nodes() {
    if (this._lastProcessedSeq < this.baseSeq + this.log.length - 1) this._processDelta();
    return this._state.nodes;
  }
  /** Fold events appended since the previous access. */
  _processDelta() {
    const tailSeq = this.baseSeq + this.log.length - 1;
    for (let seq = this._lastProcessedSeq + 1; seq <= tailSeq; seq++) {
      const index = seq - this.baseSeq;
      const event = this.log[index];
      const pending = this._pendingPlan;
      if (pending?.event === event && pending.expectedSeq === seq) {
        applySurfacePlan(this._state, pending.plan);
      } else {
        applySurfaceEvent(this._state, event, seq, this.log, this.baseSeq);
      }
      if (pending !== void 0 && pending.expectedSeq <= seq) this._pendingPlan = void 0;
      this._lastProcessedSeq = seq;
    }
  }
};

// packages/core/session/src/request-header.ts
import { callConfigEquals } from "@deepseek-ai/dsh-llm";
function canonicalHeader(header) {
  const adapterDefaults = header.adapterDefaults;
  return {
    config: header.config,
    ...adapterDefaults?.reasoningEffort === true || adapterDefaults?.maxTokens === true ? { adapterDefaults } : {},
    ...header.system !== void 0 && header.system.length > 0 ? { system: header.system } : {},
    ...header.tools !== void 0 && header.tools.length > 0 ? { tools: header.tools } : {}
  };
}
function sameSchema(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}
function headerEquals(a, b) {
  if (!callConfigEquals(a.config, b.config) || a.adapterDefaults?.reasoningEffort !== b.adapterDefaults?.reasoningEffort || a.adapterDefaults?.maxTokens !== b.adapterDefaults?.maxTokens || a.system !== b.system) return false;
  const at = a.tools ?? [];
  const bt = b.tools ?? [];
  return at.length === bt.length && at.every((tool, i) => sameSchema(tool, bt[i]));
}
function foldRequestHeader(events, from) {
  let state = from;
  for (const event of events) {
    if (event.type === "request/header") state = canonicalHeader(event.data.header);
  }
  return state;
}

// packages/core/session/src/preparation.ts
var SessionPreparation = class _SessionPreparation {
  constructor(session, options) {
    this.options = options;
    this.session = session;
  }
  options;
  released = false;
  /** The exact Session to use for setup and publication. */
  session;
  /**
   * Wrap an unpublished Session in one preparation lifetime.
   * @param session - exact unpublished Session.
   * @param options - optional provider release behavior.
   * @returns a preparation disposed after publication or rollback.
   */
  static create(session, options) {
    return new _SessionPreparation(session, options ?? {});
  }
  /** Release provider state once when this preparation leaves its caller. */
  [Symbol.dispose]() {
    if (this.released) return;
    this.released = true;
    this.options.release?.();
  }
};

// packages/core/session/src/repair.ts
import { MessageId, freezeMessage } from "@deepseek-ai/dsh-llm";
var TOOL_NOT_STARTED = "TOOL_NOT_STARTED";
var TOOL_OUTCOME_UNKNOWN = "TOOL_OUTCOME_UNKNOWN";
function interruptedTurnClosers(events) {
  let openTurn = null;
  let openStep = null;
  const pendingCalls = /* @__PURE__ */ new Map();
  for (const event of events) {
    switch (event.type) {
      case "turn/start":
        openTurn = event.data.turn;
        openStep = null;
        pendingCalls.clear();
        break;
      case "turn/end":
        openTurn = null;
        openStep = null;
        pendingCalls.clear();
        break;
      case "step/start":
        openStep = event.data.step;
        break;
      case "step/end":
        pendingCalls.clear();
        openStep = null;
        break;
      case "assistant/message":
        for (const block of event.data.message.content) {
          if (block.type === "tool-call") pendingCalls.set(block.id, { step: event.data.step });
        }
        break;
      case "tool/call":
        {
          const entry = pendingCalls.get(event.data.callId);
          if (entry) {
            entry.callSeq = event.seq;
          }
        }
        break;
      case "tool/result":
        pendingCalls.delete(event.data.message.source.callId);
        break;
      // Other event types do not move the turn/step boundary cursor.
      default:
        break;
    }
  }
  const last = events.at(-1);
  if (openTurn === null || last === void 0) return [];
  let seq = last.seq + 1;
  const time = last.time;
  const closers = [];
  for (const [callId, { step, callSeq }] of pendingCalls) {
    const started = callSeq !== void 0;
    const message = freezeMessage({
      id: MessageId(`interrupted-tool-result-${callId}-${seq}`),
      role: "user",
      source: { kind: "tool", callId },
      content: [{
        type: "tool-result",
        toolCallId: callId,
        isError: true,
        content: [{
          type: "text",
          text: started ? "The tool call was interrupted after it was recorded, but no result was durably recorded. Its outcome is unknown. Decide whether to retry from the tool semantics: retry only if the operation is read-only or idempotent; if it may have side effects, first verify external state or ask the user. Do not retry blindly." : "The tool call was interrupted before the Harness recorded it as started. Retry it if it is still needed."
        }]
      }]
    });
    closers.push({
      type: "tool/result",
      seq: seq++,
      time,
      data: {
        turn: openTurn,
        step,
        message,
        error: started ? { name: "ToolOutcomeUnknownError", code: TOOL_OUTCOME_UNKNOWN } : { name: "ToolNotStartedError", code: TOOL_NOT_STARTED }
      },
      surfaceOp: "append",
      ...started ? { sourceEventSeqs: [callSeq] } : {}
    });
  }
  if (openStep !== null) {
    closers.push({ type: "step/end", seq: seq++, time, data: { turn: openTurn, step: openStep } });
  }
  closers.push({ type: "turn/end", seq: seq++, time, data: { turn: openTurn, reason: { kind: "interrupted" } } });
  return closers;
}

// packages/core/session/src/chunk-rows.ts
import { CallId, assertNever } from "@deepseek-ai/dsh-llm";
var MIN_RUN = 3;
function isRecord(value) {
  return typeof value === "object" && value !== null;
}
function hasExactKeys(value, keys) {
  return Object.keys(value).length === keys.length && keys.every((k) => Object.hasOwn(value, k));
}
function classify(event) {
  if (event.type !== "assistant/chunk") return void 0;
  if (!hasExactKeys(event, ["type", "seq", "time", "data"])) return void 0;
  if (!Number.isSafeInteger(event.seq) || event.seq < 0 || !Number.isSafeInteger(event.time)) return void 0;
  const data = event.data;
  if (!isRecord(data) || !hasExactKeys(data, ["turn", "step", "chunk"])) return void 0;
  if (typeof data.turn !== "number" || typeof data.step !== "number") return void 0;
  const chunk = data.chunk;
  if (!isRecord(chunk) || typeof chunk.index !== "number") return void 0;
  switch (chunk.type) {
    case "text-delta":
    case "reasoning-delta":
      return hasExactKeys(chunk, ["type", "index", "text"]) && typeof chunk.text === "string" ? chunk.type : void 0;
    case "tool-call-delta": {
      const shapeOk = hasExactKeys(chunk, ["type", "index", "id", "argumentsDelta"]) || hasExactKeys(chunk, ["type", "index", "id", "name", "argumentsDelta"]) && typeof chunk.name === "string";
      return shapeOk && typeof chunk.id === "string" && typeof chunk.argumentsDelta === "string" ? chunk.type : void 0;
    }
    // Whitelist fall-through over parsed data: block-start/end, usage, finish,
    // and any future chunk variant stay one event per line.
    default:
      return void 0;
  }
}
function toolCallOf(event) {
  return event.data.chunk;
}
function indexOf(event) {
  return event.data.chunk.index;
}
function continues(prev, next, kind) {
  if (next.seq !== prev.seq + 1) return false;
  if (!Number.isSafeInteger(next.time - prev.time)) return false;
  if (next.data.turn !== prev.data.turn || next.data.step !== prev.data.step) return false;
  if (indexOf(next) !== indexOf(prev)) return false;
  if (kind !== "tool-call-delta") return true;
  const a = toolCallOf(prev);
  const b = toolCallOf(next);
  return a.id === b.id && Object.hasOwn(a, "name") === Object.hasOwn(b, "name") && a.name === b.name;
}
function buildRow(kind, run) {
  const first = run[0];
  const base = {
    turn: first.data.turn,
    step: first.data.step,
    index: indexOf(first),
    dt: run.slice(1).map((event, i) => event.time - run[i].time)
  };
  const envelope = { seq0: first.seq, time0: first.time };
  if (kind === "tool-call-delta") {
    const call = toolCallOf(first);
    return {
      type: "tool-call-chunks",
      ...envelope,
      data: {
        ...base,
        id: CallId(call.id),
        ...Object.hasOwn(call, "name") ? { name: call.name } : {},
        args: run.map((event) => event.data.chunk.argumentsDelta)
      }
    };
  }
  const data = { ...base, texts: run.map((event) => event.data.chunk.text) };
  return kind === "text-delta" ? { type: "text-chunks", ...envelope, data } : { type: "reasoning-chunks", ...envelope, data };
}
function packChunkRuns(events) {
  const out = [];
  let kind;
  let run = [];
  const flush = () => {
    if (kind !== void 0 && run.length >= MIN_RUN) out.push(buildRow(kind, run));
    else out.push(...run);
    kind = void 0;
    run = [];
  };
  for (const event of events) {
    const k = classify(event);
    if (k === void 0) {
      flush();
      out.push(event);
      continue;
    }
    const delta = event;
    const last = run[run.length - 1];
    if (k === kind && last !== void 0 && continues(last, delta, k)) {
      run.push(delta);
      continue;
    }
    flush();
    kind = k;
    run = [delta];
  }
  flush();
  return out;
}
function malformed(tag, why) {
  throw new Error(`malformed ${tag} storage row: ${why}`);
}
function validateRunData(tag, data, payloadKey) {
  if (typeof data.turn !== "number" || typeof data.step !== "number" || typeof data.index !== "number") {
    malformed(tag, "turn/step/index must be numbers");
  }
  const payload = data[payloadKey];
  if (!Array.isArray(payload) || payload.length === 0 || payload.some((entry) => typeof entry !== "string")) {
    malformed(tag, `${payloadKey} must be a non-empty string array`);
  }
  const dt = data.dt;
  if (!Array.isArray(dt) || dt.some((gap) => !Number.isSafeInteger(gap))) {
    malformed(tag, "dt must be an array of safe integers");
  }
  if (dt.length !== payload.length - 1) {
    malformed(tag, `dt length ${dt.length} does not match ${payload.length} members`);
  }
  return payload;
}
function validateRow(value, tag) {
  if (!hasExactKeys(value, ["type", "seq0", "time0", "data"])) {
    malformed(tag, "envelope must be exactly {type, seq0, time0, data}");
  }
  if (!Number.isSafeInteger(value.seq0) || value.seq0 < 0) {
    malformed(tag, "seq0 must be a non-negative safe integer");
  }
  if (!Number.isSafeInteger(value.time0)) {
    malformed(tag, "time0 must be a safe integer");
  }
  const data = value.data;
  if (!isRecord(data)) malformed(tag, "data must be an object");
  let payload;
  if (tag === "tool-call-chunks") {
    const withName = hasExactKeys(data, ["turn", "step", "index", "id", "name", "dt", "args"]);
    if (!withName && !hasExactKeys(data, ["turn", "step", "index", "id", "dt", "args"])) {
      malformed(tag, "data must be exactly {turn, step, index, id, name?, dt, args}");
    }
    if (typeof data.id !== "string" || withName && typeof data.name !== "string") {
      malformed(tag, "id (and name when present) must be strings");
    }
    payload = validateRunData(tag, data, "args");
  } else {
    if (!hasExactKeys(data, ["turn", "step", "index", "dt", "texts"])) {
      malformed(tag, "data must be exactly {turn, step, index, dt, texts}");
    }
    payload = validateRunData(tag, data, "texts");
  }
  if (!Number.isSafeInteger(value.seq0 + payload.length - 1)) {
    malformed(tag, "member seqs must stay safe integers");
  }
  let time = value.time0;
  for (const gap of data.dt) {
    time += gap;
    if (!Number.isSafeInteger(time)) malformed(tag, "member times must stay safe integers");
  }
  return value;
}
function expandRow(row) {
  const members = row.type === "tool-call-chunks" ? row.data.args : row.data.texts;
  const events = [];
  let time = row.time0;
  for (let k = 0; k < members.length; k++) {
    if (k > 0) time += row.data.dt[k - 1];
    let chunk;
    switch (row.type) {
      case "text-chunks":
        chunk = { type: "text-delta", index: row.data.index, text: members[k] };
        break;
      case "reasoning-chunks":
        chunk = { type: "reasoning-delta", index: row.data.index, text: members[k] };
        break;
      case "tool-call-chunks":
        chunk = {
          type: "tool-call-delta",
          index: row.data.index,
          id: row.data.id,
          ...Object.hasOwn(row.data, "name") ? { name: row.data.name } : {},
          argumentsDelta: members[k]
        };
        break;
      /* v8 ignore next 2 -- validateRow only returns the three row tags */
      default:
        return assertNever(row, "chunk-rows expandRow");
    }
    events.push({
      type: "assistant/chunk",
      seq: row.seq0 + k,
      time,
      data: { turn: row.data.turn, step: row.data.step, chunk }
    });
  }
  return events;
}
function decodeStorageRecord(value) {
  if (!isRecord(value)) return [value];
  const tag = value.type;
  if (tag !== "text-chunks" && tag !== "reasoning-chunks" && tag !== "tool-call-chunks") {
    return [value];
  }
  return expandRow(validateRow(value, tag));
}

// packages/core/session/src/known-event-types.ts
var KNOWN_SESSION_EVENT_TYPES = /* @__PURE__ */ new Set([
  "agent-preset/selected",
  "agent/inbox/spliced",
  "approval/asked",
  "approval/decided",
  "approval/policy",
  "assistant/chunk",
  "assistant/message",
  "command/done",
  "command/run",
  "compaction/end",
  "compaction/prune",
  "compaction/start",
  "compaction/summary",
  "feedback/record",
  "goal/change",
  "hook/invoked",
  "hook/result",
  "llm/retry",
  "llm/retry-started",
  "permission/preset",
  "plan/mode",
  "request/context",
  "request/header",
  "sandbox/mode",
  "schedule/change",
  "session/end-seed",
  "session/title",
  "session/title-llm-request",
  "step/end",
  "step/start",
  "subagent/descriptor",
  "todo/write",
  "tool-workflow/agent-end",
  "tool-workflow/agent-start",
  "tool-workflow/run-end",
  "tool-workflow/run-start",
  "tool/call",
  "tool/code-dispatch",
  "tool/code-dispatch-start",
  "tool/result",
  "turn/end",
  "turn/start",
  "user/message",
  "web/deepseek-search-llm-request"
]);

// packages/core/session/src/index.ts
function validateSessionHeader(id, input) {
  if (input === null || typeof input !== "object" || Array.isArray(input)) {
    throw new Error("session header is not a plain JSON record");
  }
  const record = input;
  if (record.version !== SESSION_FORMAT_VERSION) {
    throw new Error(`session header version must be ${SESSION_FORMAT_VERSION}, got ${String(record.version)}`);
  }
  if (record.id !== id) {
    throw new Error(`session header id "${String(record.id)}" does not match session id "${id}"`);
  }
  if (typeof record.createdAt !== "number" || !Number.isSafeInteger(record.createdAt) || record.createdAt < 0) {
    throw new Error("session header createdAt must be a non-negative safe integer");
  }
  if (record.cwd !== void 0) {
    if (typeof record.cwd !== "string") throw new Error("session header cwd must be a string");
    if (!isAbsolute(record.cwd)) {
      throw new Error(`session header cwd must be an absolute path, got "${record.cwd}"`);
    }
  }
  if (record.parentSession !== void 0 && typeof record.parentSession !== "string") {
    throw new Error("session header parentSession must be a string");
  }
  if (record.seedLength !== void 0 && (typeof record.seedLength !== "number" || !Number.isSafeInteger(record.seedLength) || record.seedLength < 0)) {
    throw new Error("session header seedLength must be a non-negative safe integer");
  }
  if (record.origin !== void 0 && record.origin !== "subagent") {
    throw new Error('session header origin must be "subagent"');
  }
  if (record.delegationDepth !== void 0 && (typeof record.delegationDepth !== "number" || !Number.isSafeInteger(record.delegationDepth) || record.delegationDepth < 0)) {
    throw new Error("session header delegationDepth must be a non-negative safe integer");
  }
  if (record.agentPreset !== void 0 && typeof record.agentPreset !== "string") {
    throw new Error("session header agentPreset must be a string");
  }
  return deepFreeze(record);
}
function validateRestoredSessionHeader(id, input) {
  if (input !== null && typeof input === "object" && !Array.isArray(input)) {
    const prototype = Reflect.getPrototypeOf(input);
    if (prototype !== Object.prototype && prototype !== null) {
      throw new Error("session header is not a plain JSON record");
    }
  }
  return validateSessionHeader(id, input);
}
function snapshotSessionHeader(id, source) {
  const input = source === void 0 ? { version: SESSION_FORMAT_VERSION, id, createdAt: Date.now() } : source;
  const snapshot = snapshotJsonValue(input);
  if (snapshot === void 0) throw new Error("session header is not losslessly JSON-serializable");
  return validateSessionHeader(id, snapshot);
}
function adoptSessionEvent(event) {
  assertMessageEventShape(
    event,
    `session event at seq ${event.seq}`
  );
  switch (event.type) {
    case "user/message":
      deepFreeze(event.data);
      break;
    case "assistant/message":
    case "tool/result":
      deepFreeze(event.data.message);
      break;
    default:
      break;
  }
  return event;
}
function snapshotSessionEvent(event) {
  return adoptSessionEvent(structuredClone(event));
}
function freezeRestoredObject(value) {
  const pending = [value];
  while (pending.length > 0) {
    const current = pending.pop();
    Object.freeze(current);
    for (const key in current) {
      const child = current[key];
      if (child !== null && typeof child === "object") pending.push(child);
    }
  }
  return value;
}
function assertSessionEventEnvelope(value, index) {
  const event = value;
  if (event["type"] === "request/header-delta") {
    throw new Error(`seed event at index ${index} uses unsupported legacy request/header-delta format`);
  }
  for (const key in event) {
    switch (key) {
      case "type":
      case "seq":
      case "time":
      case "data":
      case "surfaceOp":
      case "sourceEventSeqs":
      case "ignorable":
        break;
      default:
        throw new Error(`seed event at index ${index} has an invalid event envelope`);
    }
  }
  const type = event["type"];
  const seq = event["seq"];
  const time = event["time"];
  if (typeof type !== "string" || typeof seq !== "number" || !Number.isSafeInteger(seq) || seq < 0 || typeof time !== "number" || !Number.isSafeInteger(time) || event["data"] === void 0 || event["ignorable"] !== void 0 && event["ignorable"] !== true) {
    throw new Error(`seed event at index ${index} has an invalid event envelope`);
  }
  switch (type) {
    case "request/header":
    case "user/message":
    case "assistant/message":
    case "tool/result":
      assertCurrentLlmShape(event, index);
      break;
  }
}
function assertCurrentLlmShape(event, index) {
  const data = event["data"];
  const record = typeof data === "object" && data !== null ? data : void 0;
  if (event["type"] === "request/header") {
    const header = record?.["header"];
    const headerRecord = typeof header === "object" && header !== null && !Array.isArray(header) ? header : void 0;
    const config = headerRecord?.["config"];
    if (!hasProviderModel(config)) throw new Error(`seed request/header at index ${index} lacks provider/model`);
    const configRecord = config;
    const reasoningEffort = configRecord["reasoningEffort"];
    if (reasoningEffort !== void 0 && (typeof reasoningEffort !== "string" || reasoningEffort.length === 0)) {
      throw new Error(`seed request/header at index ${index} has an invalid reasoningEffort`);
    }
    assertAdapterDefaults(headerRecord?.["adapterDefaults"], configRecord, index);
  }
  const type = event["type"];
  if (type !== "user/message" && type !== "assistant/message" && type !== "tool/result") return;
  assertMessageEventShape(event, `seed ${type} at index ${index}`);
}
var allowedAdapterKeys = /* @__PURE__ */ new Set(["reasoningEffort", "maxTokens"]);
function assertAdapterDefaults(value, config, index) {
  if (value === void 0) return;
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`seed request/header at index ${index} has invalid adapterDefaults`);
  }
  const defaults = value;
  if (Object.keys(defaults).some((key) => !allowedAdapterKeys.has(key)) || Object.values(defaults).some((marker) => marker !== true) || defaults["reasoningEffort"] === true && config["reasoningEffort"] === void 0 || defaults["maxTokens"] === true && config["maxTokens"] === void 0) {
    throw new Error(`seed request/header at index ${index} has invalid adapterDefaults`);
  }
}
function assertMessageEventShape(event, subject) {
  const type = event["type"];
  if (type !== "user/message" && type !== "assistant/message" && type !== "tool/result") return;
  const data = event["data"];
  const record = typeof data === "object" && data !== null ? data : void 0;
  const message = type === "user/message" ? record : record?.["message"];
  if (typeof message !== "object" || message === null || typeof message["id"] !== "string" || message["id"] === "") {
    throw new Error(`${subject} lacks an identified message`);
  }
  const messageRecord = message;
  const expectedRole = type === "assistant/message" ? "assistant" : "user";
  if (messageRecord["role"] !== expectedRole) {
    throw new Error(`${subject} message must have role "${expectedRole}"`);
  }
  const source = messageRecord["source"];
  if (typeof source !== "object" || source === null || typeof source["kind"] !== "string" || source["kind"] === "") {
    throw new Error(`${subject} message has invalid source`);
  }
  if (!Array.isArray(messageRecord["content"])) {
    throw new Error(`${subject} message has invalid content`);
  }
  const sourceRecord = source;
  if (type === "assistant/message") {
    if (sourceRecord["kind"] !== "model" || !hasProviderModel(sourceRecord)) {
      throw new Error(`${subject} message must have model source`);
    }
    return;
  }
  if (type !== "tool/result") return;
  if (sourceRecord["kind"] !== "tool" || typeof sourceRecord["callId"] !== "string" || sourceRecord["callId"] === "") {
    throw new Error(`${subject} message must have tool source`);
  }
  const content = messageRecord["content"];
  const block = content[0];
  if (content.length !== 1 || typeof block !== "object" || block === null || block["type"] !== "tool-result" || !Array.isArray(block["content"])) {
    throw new Error(`${subject} message must contain one tool-result block`);
  }
  if (block["toolCallId"] !== sourceRecord["callId"]) {
    throw new Error(`${subject} message has mismatched tool call ids`);
  }
}
function hasProviderModel(value) {
  if (typeof value !== "object" || value === null) return false;
  const pair = value;
  return typeof pair["provider"] === "string" && pair["provider"].length > 0 && typeof pair["model"] === "string" && pair["model"].length > 0;
}
function assertSupportedRequestHeader(type, data, location) {
  if (type === "request/header-delta") {
    throw new Error(`${location} uses unsupported legacy request/header-delta format`);
  }
  if (type === "request/header" && data !== null && typeof data === "object" && !Array.isArray(data) && data["reason"] === "fallback") {
    throw new Error(`${location} uses unsupported legacy request/header reason "fallback"`);
  }
}
function collectSessionCallbacks(ctx, args) {
  return [...ctx.events.dispatch("emit", args)];
}
function invokeContainedSessionObservers(ctx, name, id, args, callbacks) {
  for (const callback of callbacks) {
    try {
      const returned = callback(...args);
      void Promise.resolve(returned).catch((error) => {
        ctx.logger.warn(`session "${id}": ${name} listener rejected: ${String(error)}`);
      });
    } catch (error) {
      ctx.logger.warn(`session "${id}": ${name} listener threw: ${String(error)}`);
    }
  }
}
var attachments = /* @__PURE__ */ new WeakMap();
var Session = class _Session {
  log = [];
  /** Single incremental owner of surface acceptance and projection state. */
  surfaceManager = new SurfaceManager(this.log);
  /** The ordered surface over this session's event log. */
  get surface() {
    return this.surfaceManager;
  }
  /**
   * Detached, deep-frozen creation metadata (format version, cwd, lineage,
   * seed boundary). Supplied by the store via `ctx.sessions.create()`. When a
   * `Session` is created without a store-owned header, a minimal header is
   * synthesized (stamped with the current {@link SESSION_FORMAT_VERSION}) so
   * `session.header` is always present. Kept out of the event log — it is a
   * storage concern, not replayable conversation state.
   */
  header;
  /** The session identity, derived from its durable header's single copy. */
  get id() {
    return this.header.id;
  }
  /**
   * The first seq appended IN THIS PROCESS: the length of the constructor
   * seed (0 without one). Events with smaller seq values entered through
   * construction — replay, fork, or resume — and were never published on the
   * `session/event` firehose (constructor seeds do not emit), so consumers
   * that replay the log as a publication substitute (telemetry adoption)
   * start here. Distinct from `header.seedLength`, the DURABLE fork-lineage
   * boundary: a resumed session's constructor seed is its full stored log,
   * while its header keeps the original fork value — this field is the
   * in-process construction fact.
   *
   * Not persisted itself: a seeded session projects it into the log as the
   * `session/end-seed` event, which is what a consumer reading STORED history
   * reads. Locate the LAST such event, not necessarily one at this seq — a
   * seed already ending in one is not re-marked, so reopening an untouched
   * session leaves that event at a smaller seq than `firstLiveSeq`. Prefer
   * this field in-process: it is exact before the marker reaches storage.
   *
   * When this lifecycle appends the marker, it occupies this seq before the
   * store attaches and therefore does not publish either. Otherwise this seq
   * holds an ordinary published write.
   */
  firstLiveSeq;
  /**
   * Create a detached session by validating and snapshotting borrowed seed
   * events and storage metadata.
   * @param id - session identity.
   * @param seed - optional borrowed replay or fork events.
   * @param header - optional borrowed storage metadata.
   * @returns a detached session.
   */
  static create(id, seed, header) {
    return new _Session(id, seed, header);
  }
  /**
   * Restore a detached session by taking ownership of fresh persistence values.
   * The storage format, event envelopes, sequence continuity, surface transitions,
   * and header fields are validated before the restored objects are frozen.
   * @param id - restored session identity.
   * @param seed - fresh detached events whose ownership is transferred.
   * @param header - fresh detached metadata whose ownership is transferred.
   * @returns a restored detached session.
   */
  static fromRestore(id, seed, header) {
    return new _Session(id, seed, header, "restore");
  }
  constructor(id, seed, header, mode = "snapshot") {
    const restoredHeader = mode === "restore" ? validateRestoredSessionHeader(id, header) : void 0;
    if (seed !== void 0) {
      for (const [index, source] of seed.entries()) {
        const snapshot = mode === "restore" ? source : snapshotJsonValue(source);
        if (snapshot === void 0) {
          throw new Error(`seed event at index ${index} is not losslessly JSON-serializable`);
        }
        assertSessionEventEnvelope(snapshot, index);
        assertSupportedRequestHeader(snapshot.type, snapshot.data, `seed event at index ${index}`);
        if (snapshot.seq !== index) {
          throw new Error(`seed event at index ${index} has seq ${snapshot.seq} (expected ${index}); seed must be contiguous from 0`);
        }
        try {
          this.surfaceManager.validateNext(snapshot);
        } catch (error) {
          throw new Error(`invalid seed event at index ${index}: ${error instanceof Error ? error.message : "invalid surface metadata"}`);
        }
        this.log.push(mode === "restore" ? freezeRestoredObject(snapshot) : deepFreeze(snapshot));
      }
    }
    this.firstLiveSeq = this.log.length;
    this.header = restoredHeader ?? snapshotSessionHeader(id, header);
    if (seed !== void 0 && this.log.at(-1)?.type !== "session/end-seed") {
      this.append("session/end-seed", {});
    }
  }
  /** Cached immutable public snapshot of the private append-only log. */
  eventsSnapshot;
  /**
   * An immutable snapshot of the append-only event log. The snapshot is reused
   * until the next append; a previously returned array does not grow later.
   * Events and their nested data are deep-frozen at acceptance, so neither a
   * cast nor ordinary JavaScript can rewrite durable history.
   */
  get events() {
    this.eventsSnapshot ??= Object.freeze([...this.log]);
    return this.eventsSnapshot;
  }
  /** The next event's sequence number — always the log length (the `seq = log.length` contiguity contract). */
  get seq() {
    return this.log.length;
  }
  /**
   * Append one typed event to the log and synchronously notify observers via
   * the store-owned, module-private publication hooks. The hot path never blocks
   * on I/O — persistence plugins buffer asynchronously. Once the event enters
   * the log, the append is committed: observer failures are logged and
   * contained per listener, so they do not change the return value or prevent
   * later listeners from observing the same accepted event.
   *
   * @param type - The event type (key of {@link SessionEventMap}).
   * @param data - The event payload; must be JSON-serializable.
   * @param opts - Surface metadata: `surfaceOp` controls how the event enters
   *   the ordered surface; `sourceEventSeqs` lists the seq numbers of earlier
   *   events this one derives from. REQUIRED for
   *   {@link SurfaceEventType} events (every message-producing event must
   *   declare how it joins the surface, the sole source of derived model
   *   history) and
   *   rejected by the compiler for non-surface types like `turn/start` or
   *   `assistant/chunk`.
   * @returns the logged event — its assigned `seq`/`time` plus the SNAPSHOT of
   *   `data` that entered the log, so reading `event.data` back sees the logged
   *   value, never the caller's still-mutable input.
   * @throws if `data` or surface metadata is not losslessly JSON-serializable
   *   (BigInt, function, symbol, undefined, negative zero, non-finite number,
   *   circular reference, sparse array, or an exotic object such as
   *   Map/Set/Date/class instance), or when the candidate violates the
   *   canonical surface contract (marker shape and eligibility, unique
   *   earlier source-event references, positional replacement validity, and complete
   *   shadowed-node coverage). One recursive pass reads, validates, and
   *   copies each nested value once, so a stateful getter cannot supply one value
   *   to validation and another to storage. The event log is the durable source
   *   of truth, so a bad event fails at the append site rather than later during
   *   a backend flush. A synchronous internal dispatch validation failure or an
   *   append reentered while this acceptance/publication boundary is open also
   *   rejects before the log changes.
   */
  append(type, data, ...opts) {
    const surfaceOpts = opts[0];
    const surfaceMetadata = {
      ...surfaceOpts?.sourceEventSeqs === void 0 ? {} : { sourceEventSeqs: surfaceOpts.sourceEventSeqs },
      ...surfaceOpts?.surfaceOp === void 0 ? {} : { surfaceOp: surfaceOpts.surfaceOp }
    };
    const dataSnapshot = snapshotJsonValue(data);
    if (dataSnapshot === void 0) {
      throw new Error(`session event "${type}" carries non-JSON-serializable data`);
    }
    assertSupportedRequestHeader(type, dataSnapshot, `session event "${type}"`);
    const surfaceMetadataSnapshot = snapshotJsonValue(surfaceMetadata);
    if (surfaceMetadataSnapshot === void 0) {
      throw new Error(`session event "${type}" carries non-JSON-serializable surface metadata`);
    }
    const entry = attachments.get(this);
    if (entry?.appending) {
      throw new Error("session append cannot reenter while another append is being published");
    }
    const event = deepFreeze({
      type,
      seq: this.log.length,
      time: Date.now(),
      data: dataSnapshot,
      ...surfaceMetadataSnapshot
    });
    this.surfaceManager.validateNext(event);
    if (entry !== void 0) entry.appending = true;
    try {
      let callbacks;
      const callbackArgs = [this, event];
      if (entry !== void 0) {
        callbacks = collectSessionCallbacks(entry.emitCtx, [entry.carrier, "session/event", ...callbackArgs]);
      }
      this.log.push(event);
      this.eventsSnapshot = void 0;
      if (callbacks !== void 0 && entry !== void 0) {
        invokeContainedSessionObservers(entry.emitCtx, "session/event", entry.id, callbackArgs, callbacks);
      }
      return event;
    } finally {
      if (entry !== void 0) {
        entry.appending = false;
        if (entry.detachRequested && !entry.announcing) entry.detach();
      }
    }
  }
  /** Cached fold of the request-header events — see {@link requestHeader}. */
  headerFold;
  /** Log position (events consumed) the header fold has reached. */
  headerFoldSeq = 0;
  /**
   * The {@link EpochHeader} in force after the log's last header event — the
   * header the NEXT request will be compared against — or undefined before
   * the first `request/header` snapshot. The live, incrementally-maintained
   * form of `foldRequestHeader(session.events)`: each header event is folded
   * once, when first seen, so a per-step read costs O(new events).
   * @returns the folded header, or undefined when no header event exists yet.
   */
  requestHeader() {
    if (this.headerFoldSeq < this.log.length) {
      this.headerFold = deepFreeze(foldRequestHeader(this.log.slice(this.headerFoldSeq), this.headerFold));
      this.headerFoldSeq = this.log.length;
    }
    return this.headerFold;
  }
  /** Cached fold of `request/context` events. */
  contextFold;
  contextFoldSeq = 0;
  /**
   * Return the latest resolved route metadata, or `undefined` before the first
   * `request/context` event. Each event is folded once.
   * @returns the latest immutable route metadata.
   */
  requestContext() {
    if (this.contextFoldSeq < this.log.length) {
      for (const event of this.log.slice(this.contextFoldSeq)) {
        if (event.type === "request/context") this.contextFold = deepFreeze({ ...event.data });
      }
      this.contextFoldSeq = this.log.length;
    }
    return this.contextFold;
  }
  /** The derived-message cache: frozen projections, extended per unseen node. */
  derived = [];
  /** Surface position (nodes projected) the cache has reached. */
  derivedNodes = 0;
  /** {@link SurfaceManager.replaceGeneration} the cache was built under. */
  derivedGeneration = 0;
  /**
   * Derive the LLM message history by walking the ordered sequences of
   * message-producing events maintained by `surfaceOp` markers. The
   * surface is the single source of derived history: every message-producing
   * append records its `surfaceOp`, so a raw event with no marker (a chunk, a
   * turn boundary) is correctly absent, and a compaction `replace` deletes the
   * shadowed nodes from the derivation. The projection rules are
   * {@link deriveEventMessage}, folded per node.
   *
   * CACHED: each surface node is projected exactly once, when first seen — a
   * call costs O(new nodes), and a surface rewrite (a `replace`;
   * {@link SessionSurface.replaceGeneration}) rebuilds. The returned array is
   * a fresh snapshot per call (later appends never grow an array a caller
   * already holds); the `Message` objects in it are SHARED and **deep-frozen**.
   * Their content reuses the already frozen durable event data, so the cache
   * needs no second deep clone and consumers still cannot mutate the log.
   * @returns a fresh array of the shared, frozen derived history.
   */
  deriveMessages() {
    const surface = this.surface;
    const nodes = surface.nodes;
    const generation = surface.replaceGeneration;
    if (generation !== this.derivedGeneration) {
      this.derived = [];
      this.derivedNodes = 0;
      this.derivedGeneration = generation;
    }
    for (const seq of nodes.slice(this.derivedNodes)) {
      const msg = this.deriveEventMessage(this.log[seq]);
      if (msg) this.derived.push(msg);
    }
    this.derivedNodes = nodes.length;
    return [...this.derived];
  }
  /**
   * Instance face of the pure per-node `deriveEventMessage` export from
   * `surface.ts`.
   * @param event - the event to project.
   * @returns the derived message, or null when the event produces none.
   */
  deriveEventMessage(event) {
    return deriveEventMessage(event);
  }
};
var SessionForkError = class extends Error {
  constructor(message, code) {
    super(message);
    this.code = code;
    this.name = "SessionForkError";
  }
  code;
};
var SessionStore = class extends Service {
  store = /* @__PURE__ */ new Map();
  counter = 0;
  constructor(ctx) {
    super(ctx, "sessions");
    ctx.inject(["typert"], (typeCtx) => {
      typeCtx.typert.lookups.register("session", {
        parameter: "session",
        wire: "sessionId",
        hostTypeSymbol: "@deepseek-ai/dsh-session#Session",
        wireTypeSymbol: "@deepseek-ai/dsh-session/types#SessionId",
        resolve: (sessionId) => this.get(sessionId)
      });
    });
  }
  /**
   * Create a session owned by the calling fiber: disposing that fiber stops
   * event notification and removes the session from the store. `options.seed`
   * populates the session with a copy of those events (replay/fork);
   * `options.meta` attaches creation metadata (validated absolute `cwd`, seed
   * and parent lineage, and delegation depth) as the immutable
   * {@link SessionHeader} (the store fills `version`/`id`/`createdAt`).
   *
   * For an agent whose session must be torn down IN ORDER with its loop (so the
   * loop's final events are published before the store attachment ends), do NOT use this
   * — fold the session lifecycle into the agent's own effect via
   * {@link prepare} + {@link enter} + {@link announce} (see
   * `dsh-agent-loop`'s creation transaction).
   *
   * @param id - the session id; omitted, the store mints `session-<n>`.
   * @param options - seed events and/or creation metadata for the header.
   * @returns the live session, already entered and announced.
   * @throws if a session with `id` already exists, metadata is not a plain
   *   lossless-JSON record with valid scalar fields, or `meta.cwd` is a
   *   non-absolute path (storage backends key directories off it).
   */
  create(id, options) {
    const session = this.prepare(id, options);
    this.ctx.effect(function* () {
      yield this.enter(session);
      this.announce(session);
    }.bind(this), "sessions.create()");
    return session;
  }
  /**
   * Build a session WITHOUT entering it into the store — validate the id/cwd and
   * construct the {@link Session} (with its immutable {@link SessionHeader}).
   * Pairs with {@link enter} + {@link announce}: a caller that owns a composite
   * `ctx.effect` (the agent factory) folds the session lifecycle into that ONE
   * effect so a fiber unload tears the session + agent down as a single ORDERED
   * chain rather than as racing sibling effects — which would remove the publication hooks
   * before the driver's closing events commit, dropping them.
   *
   * @param id - the session id; omitted, the store mints `session-<n>`.
   * @param options - seed events and/or creation metadata for the header. With
   *   `seedSource: 'persistence'`, metadata and events must be fresh detached
   *   graphs whose ownership transfers to this call: they are validated and
   *   frozen in place through {@link Session.fromRestore}, so the caller must
   *   retain no mutable aliases.
   * @returns the constructed session, NOT yet in the store.
   * @throws if a session with `id` already exists, metadata is not a plain
   *   lossless-JSON record with valid scalar fields, or `meta.cwd` is a
   *   non-absolute path.
   */
  prepare(id, options) {
    let sessionId;
    if (id === void 0) {
      do
        sessionId = SessionId(`session-${++this.counter}`);
      while (this.store.has(sessionId));
    } else {
      sessionId = SessionId(id);
    }
    if (this.store.has(sessionId)) throw new Error(`session "${sessionId}" already exists`);
    if (options?.seedSource === "persistence") {
      return Session.fromRestore(sessionId, options.seed, options.meta);
    }
    const seed = options?.seed;
    const meta = options?.meta;
    const header = {
      version: SESSION_FORMAT_VERSION,
      id: sessionId,
      createdAt: meta?.createdAt ?? Date.now(),
      ...meta?.cwd === void 0 ? {} : { cwd: meta.cwd },
      ...meta?.parentSession === void 0 ? {} : { parentSession: meta.parentSession },
      ...meta?.seedLength === void 0 ? {} : { seedLength: meta.seedLength },
      ...meta?.origin === void 0 ? {} : { origin: meta.origin },
      ...meta?.delegationDepth === void 0 ? {} : { delegationDepth: meta.delegationDepth },
      ...meta?.agentPreset === void 0 ? {} : { agentPreset: meta.agentPreset }
    };
    return Session.create(sessionId, seed, header);
  }
  /**
   * Enter a {@link prepare}d session into the store: install the module-private
   * append publication hooks and add it to the store. Returns the DETACH
   * disposer (hooks + store removal). Does NOT emit `session/created` —
   * the caller yields this disposer inside its effect and THEN calls
   * {@link announce}, so a throwing `session/created` listener rolls the attach
   * back instead of leaking it.
   *
   * Re-checks the id for a duplicate: `prepare` and `enter` are public
   * cross-package primitives and a caller may interleave arbitrary work (or
   * another create) between them, so a stale prepared session must NOT overwrite
   * a live store entry of the same id — its detach disposer would later delete
   * the REAL session. The {@link create} convenience and the agent factory call
   * the two back-to-back so they never trip this, but the public API cannot
   * assume that.
   *
   * @param session - a {@link prepare}d session not yet in the store.
   * @returns the detach disposer (publication hooks + store removal). When called from
   *   a synchronous `session/created` listener, removal and disposal wait until
   *   that creation dispatch unwinds.
   * @throws if a session with this id is already in the store.
   */
  enter(session) {
    const id = session.id;
    const carrier = scopeTarget(session, scopeOf(this.ctx));
    if (this.store.has(id)) throw new Error(`session "${id}" already exists`);
    if (attachments.has(session)) throw new Error(`session "${id}" is already attached to a store`);
    const entry = {
      id,
      session,
      carrier,
      emitCtx: this.ctx,
      announced: false,
      announcing: false,
      appending: false,
      detachRequested: false,
      detach: () => {
        this.detachEntered(entry);
      }
    };
    this.store.set(id, entry);
    attachments.set(session, entry);
    let entered = true;
    const detach = () => {
      if (!entered) return;
      entered = false;
      if (entry.announcing || entry.appending) {
        entry.detachRequested = true;
        return;
      }
      entry.detach();
    };
    return detach;
  }
  /** Remove one exact entered session and emit its paired disposal when announced. */
  detachEntered(entry) {
    entry.detachRequested = false;
    if (this.store.get(entry.id) !== entry) return;
    this.store.delete(entry.id);
    attachments.delete(entry.session);
    if (entry.announced) this.emitDisposed(entry);
  }
  /** Emit `session/created` exactly once for an {@link enter}ed session (with
   * the carrier {@link enter} captured). Separate from {@link enter} so the
   * caller can yield the detach disposer first (rollback safety — see
   * {@link enter}).
   * @param session - the entered session to announce to listeners.
   * @throws if the session is not live or its announcement already began,
   *   including a reentrant call from a creation listener. */
  announce(session) {
    const entry = this.liveEntryFor(session);
    if (entry.announced || entry.announcing) {
      throw new Error(`session "${entry.id}" was already announced`);
    }
    entry.announced = true;
    const callbackArgs = [session];
    entry.announcing = true;
    try {
      const callbacks = collectSessionCallbacks(this.ctx, [entry.carrier, "session/created", session]);
      for (const callback of callbacks) {
        const returned = callback(...callbackArgs);
        void Promise.resolve(returned).catch((error) => {
          this.ctx.logger.warn(`session "${entry.id}": session/created listener rejected: ${String(error)}`);
        });
      }
    } finally {
      entry.announcing = false;
      if (entry.detachRequested && !entry.appending) entry.detach();
    }
  }
  /** Emit the paired teardown notification with per-listener containment. */
  emitDisposed(entry) {
    const callbackArgs = [entry.session];
    try {
      const callbacks = collectSessionCallbacks(this.ctx, [entry.carrier, "session/disposed", entry.session]);
      invokeContainedSessionObservers(this.ctx, "session/disposed", entry.id, callbackArgs, callbacks);
    } catch (error) {
      this.ctx.logger.warn(`session "${entry.id}": session/disposed dispatch threw: ${String(error)}`);
    }
  }
  /**
   * Dispatch the awaited `session/flush` durability checkpoint for `session`,
   * with the carrier captured at {@link enter}. THE flush entry point: the
   * store owns the carrier, so callers (the checkpoint policy's per-request
   * barrier, goal-round-driver's idle checkpoint, teardown drains, and consumers
   * that flush themselves before reading storage) must come through here
   * rather than dispatch a raw `ctx.parallel('session/flush', …)` — one owner,
   * one spelling, and the scoped-dispatch invariant can pin it.
   * @param session - the session whose buffered events must reach durable storage.
   * @returns whether at least one durability listener participated, after every
   *   listener has settled successfully.
   * @throws the first registered listener failure after every listener settles.
   */
  async flush(session) {
    const { carrier } = this.liveEntryFor(session);
    const callbackArgs = [session];
    const callbacks = collectSessionCallbacks(this.ctx, [carrier, "session/flush", session]);
    const results = await Promise.allSettled(callbacks.map((callback) => {
      try {
        return callback(...callbackArgs);
      } catch (error) {
        return Promise.reject(error);
      }
    }));
    const failure = results.find((result) => result.status === "rejected");
    if (failure !== void 0) throw failure.reason;
    return callbacks.length > 0;
  }
  /** Return the exact live entry; detached/prepared objects reject. */
  liveEntryFor(session) {
    const entry = attachments.get(session);
    if (entry === void 0 || this.store.get(entry.id) !== entry) {
      throw new Error(`session "${session.id}" is not live in this store`);
    }
    return entry;
  }
  /**
   * Look up a live session.
   * @param id - the session id to look up.
   * @returns the session, or undefined when no live session has that id.
   */
  get(id) {
    return this.store.get(id)?.session;
  }
  /**
   * All live sessions, in creation order.
   * @returns a fresh array; mutating it does not affect the store.
   */
  list() {
    return [...this.store.values()].map((entry) => entry.session);
  }
  /**
   * Create a live child session from a stable prefix of a live source.
   * `boundary` is an inclusive source event seq; omitted means the source's
   * current last event. The selected slice may end with a between-turn event
   * but must not end inside an open turn.
   *
   * @param source - Live source session object or id.
   * @param boundary - Inclusive source event seq to fork through; omitted means
   *   the source's current last event, and omitted on an empty source forks an
   *   empty child.
   * @param childSessionId - Optional child session id; omitted delegates to
   *   `SessionStore`'s id policy.
   * @returns The created live child session.
   */
  fork(source, boundary, childSessionId) {
    if (childSessionId !== void 0 && this.get(childSessionId) !== void 0) {
      throw new SessionForkError(`session "${childSessionId}" already exists`, "SESSION_ALREADY_EXISTS");
    }
    const liveSource = this._resolveForkSource(source);
    const seed = this._forkSeed(liveSource, boundary);
    return this.create(childSessionId, {
      seed,
      meta: {
        ...liveSource.header.cwd !== void 0 ? { cwd: liveSource.header.cwd } : {},
        parentSession: liveSource.id,
        seedLength: seed.length
      }
    });
  }
  _forkSeed(session, requestedBoundary) {
    const events = session.events;
    const lastEvent = events.at(-1);
    let boundary;
    if (requestedBoundary !== void 0) {
      boundary = requestedBoundary;
    } else {
      if (lastEvent === void 0) return [];
      boundary = lastEvent.seq;
    }
    if (!Number.isSafeInteger(boundary) || boundary < 0) {
      throw new SessionForkError(
        `fork boundary for session "${session.id}" must be a non-negative safe integer, got ${String(boundary)}`,
        "INVALID_BOUNDARY"
      );
    }
    if (boundary >= events.length) {
      const lastSeq = events.at(-1)?.seq;
      throw new SessionForkError(
        `fork boundary ${boundary} does not exist in session "${session.id}" (last seq: ${lastSeq ?? "none"})`,
        "INVALID_BOUNDARY"
      );
    }
    const boundaryEvent = events[boundary];
    if (boundaryEvent === void 0 || boundaryEvent.seq !== boundary) {
      throw new SessionForkError(
        `fork boundary ${boundary} does not match a contiguous event seq in session "${session.id}"`,
        "INVALID_BOUNDARY"
      );
    }
    const lastTurnBoundary = events.slice(0, boundary + 1).findLast((event) => event.type === "turn/start" || event.type === "turn/end");
    if (lastTurnBoundary?.type === "turn/start") {
      throw new SessionForkError(
        `fork boundary ${boundary} in session "${session.id}" ends inside open turn ${lastTurnBoundary.data.turn}`,
        "OPEN_TURN"
      );
    }
    return events.slice(0, boundary + 1);
  }
  _resolveForkSource(source) {
    if (typeof source === "string") {
      const session = this.get(source);
      if (session === void 0) throw new SessionForkError(`session "${source}" not found`, "SESSION_NOT_FOUND");
      return session;
    }
    const live = this.get(source.id);
    if (live === void 0) {
      throw new SessionForkError(`session "${source.id}" not found`, "SESSION_NOT_FOUND");
    }
    if (live !== source) throw new SessionForkError(`session "${source.id}" is not the live store instance`, "SESSION_NOT_LIVE");
    return source;
  }
};
var index_default = SessionStore;
export {
  KNOWN_SESSION_EVENT_TYPES,
  SESSION_FORMAT_VERSION,
  Session,
  SessionForkError,
  SessionId,
  SessionPreparation,
  SessionStore,
  TOOL_NOT_STARTED,
  TOOL_OUTCOME_UNKNOWN,
  adoptSessionEvent,
  canonicalHeader,
  decodeStorageRecord,
  index_default as default,
  deriveEventMessage,
  foldRequestHeader,
  foldSurface,
  headerEquals,
  interruptedTurnClosers,
  isAppendSurfaceEvent,
  isJsonValue,
  isReplacementSurfaceEvent,
  isSurfaceEligibleType,
  isSurfaceEvent,
  packChunkRuns,
  snapshotJsonValue,
  snapshotSessionEvent
};
