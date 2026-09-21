var __knownSymbol = (name, symbol) => (symbol = Symbol[name]) ? symbol : /* @__PURE__ */ Symbol.for("Symbol." + name);
var __typeError = (msg) => {
  throw TypeError(msg);
};
var __using = (stack, value, async) => {
  if (value != null) {
    if (typeof value !== "object" && typeof value !== "function") __typeError("Object expected");
    var dispose, inner;
    if (async) dispose = value[__knownSymbol("asyncDispose")];
    if (dispose === void 0) {
      dispose = value[__knownSymbol("dispose")];
      if (async) inner = dispose;
    }
    if (typeof dispose !== "function") __typeError("Object not disposable");
    if (inner) dispose = function() {
      try {
        inner.call(this);
      } catch (e) {
        return Promise.reject(e);
      }
    };
    stack.push([async, dispose, value]);
  } else if (async) {
    stack.push([async]);
  }
  return value;
};
var __callDispose = (stack, error, hasError) => {
  var E = typeof SuppressedError === "function" ? SuppressedError : function(e, s, m, _) {
    return _ = Error(m), _.name = "SuppressedError", _.error = e, _.suppressed = s, _;
  };
  var fail = (e) => error = hasError ? new E(e, error, "An error was suppressed during disposal") : (hasError = true, e);
  var next = (it) => {
    while (it = stack.pop()) {
      try {
        var result = it[1] && it[1].call(it[2]);
        if (it[0]) return Promise.resolve(result).then(next, (e) => (fail(e), next()));
      } catch (e) {
        fail(e);
      }
    }
    if (hasError) throw error;
  };
  return next();
};

// packages/core/agent-loop/src/index.ts
import { FiberState, Service } from "@deepseek-ai/cordis";
import { randomUUID } from "node:crypto";
import z from "@deepseek-ai/schemastery";
import { emitAgentEvent } from "@deepseek-ai/dsh-agent";
import { errorChain as errorChain2 } from "@deepseek-ai/dsh-llm";
import { installSettingsSection, settingsNamespace } from "@deepseek-ai/dsh-settings";
import { SessionId, SessionPreparation } from "@deepseek-ai/dsh-session";

// packages/core/agent-loop/src/agent.ts
import { Inbox, agentEvents, assembleContextFor } from "@deepseek-ai/dsh-agent";
import {
  BlockAssembler,
  LlmError,
  createAssistantMessage,
  deepFreeze,
  errorChain,
  markAgentLoopRequest
} from "@deepseek-ai/dsh-llm";
import { createScope } from "@deepseek-ai/dsh-scope";
import { canonicalHeader, headerEquals } from "@deepseek-ai/dsh-session";
import { joinContextSections, renderContextSections, renderPrompt } from "@deepseek-ai/dsh-system-prompt";

// packages/core/agent-loop/src/runtime-context.ts
import { createUserMessage } from "@deepseek-ai/dsh-llm";
import { isReplacementSurfaceEvent } from "@deepseek-ai/dsh-session";
var SOURCE = "@deepseek-ai/dsh-system-prompt";
var CLEARED = "Current runtime context: none. Earlier runtime-context snapshots no longer apply.";
function isOwned(message) {
  return message.source.kind === "plugin" && message.source.plugin === SOURCE;
}
function textOf(message) {
  const [block] = message.content;
  return message.content.length === 1 && block?.type === "text" ? block.text : void 0;
}
var RuntimeContextProjection = class {
  /** `undefined` means no snapshot ever existed; `null` means none is retained. */
  retained;
  /**
   * Restore projection state once, then follow authoritative session events.
   * @param ctx - agent-scoped event context.
   * @param session - session receiving projected messages.
   */
  constructor(ctx, session) {
    const surface = new Set(session.surface.nodes);
    for (let index = session.events.length - 1; index >= 0; index -= 1) {
      const event = session.events[index];
      if (event?.type !== "user/message" || !isOwned(event.data)) continue;
      this.retained ??= null;
      if (surface.has(event.seq)) {
        this.retained = { seq: event.seq, text: textOf(event.data) };
        break;
      }
    }
    ctx.on("session/event", (subject, event) => {
      if (subject !== session) return;
      if (event.type === "user/message" && isOwned(event.data)) {
        this.retained = { seq: event.seq, text: textOf(event.data) };
      } else if (this.retained && isReplacementSurfaceEvent(event) && event.sourceEventSeqs?.includes(this.retained.seq) === true) {
        this.retained = null;
      }
    });
  }
  /**
   * Create an uncommitted snapshot only when the retained value differs.
   * @param current - fully rendered dynamic context.
   * @param sections - named contributions that formed the current snapshot.
   * @returns a candidate user message, or `undefined` when no update is needed.
   */
  project(current, sections) {
    if (this.retained === void 0 && current.length === 0) return;
    const snapshot = current.length === 0 ? CLEARED : current;
    if (this.retained?.text === snapshot) return;
    return createUserMessage({
      content: [{ type: "text", text: snapshot }],
      // The cleared marker has no contributions left to attribute.
      source: sections.length === 0 ? { kind: "plugin", plugin: SOURCE } : { kind: "plugin", plugin: SOURCE, form: "snapshot", sections }
    });
  }
};

// packages/core/agent-loop/src/tool-calls.ts
import { assertNever, createToolResultMessage } from "@deepseek-ai/dsh-llm";
import { TOOL_ABORTED_BEFORE_DISPATCH, TOOL_RUNTIME_SCHEDULER } from "@deepseek-ai/dsh-tools";
async function executeToolCalls(ctx, turn, step, toolCalls, signal, acceptContext) {
  const agent = ctx.agents.requireInitiator();
  const { session } = agent;
  const planned = toolCalls.map((block) => ({
    block,
    exec: {
      callId: block.id,
      name: block.name,
      arguments: parseArguments(block.arguments),
      agent,
      signal
    }
  }));
  let next = 0;
  let concluded = false;
  while (next < planned.length) {
    const first = planned[next];
    const mode = ctx.tools.executionMode(first.exec).kind;
    const group = mode === "parallel" ? planned.slice(next) : [first];
    const outcome = await runGroup(
      ctx,
      turn,
      step,
      group,
      mode,
      signal,
      acceptContext
    );
    next += outcome.consumed;
    concluded ||= outcome.concluded;
    if (outcome.aborted) {
      for (const call of planned.slice(next)) appendSkippedToolCall(session, turn, step, call.block);
      return { concluded };
    }
  }
  return { concluded };
}
function parseArguments(raw) {
  try {
    return raw ? JSON.parse(raw) : {};
  } catch {
    return raw;
  }
}
async function runGroup(ctx, turn, step, group, mode, signal, acceptContext) {
  const { session } = ctx.agents.requireInitiator();
  const { maxParallelToolCalls } = ctx.agentLoop.config;
  const slots = group.map(() => void 0);
  const callSeqs = group.map(() => -1);
  let nextToStart = 0;
  let committed = 0;
  let started = 0;
  let aborted = signal.aborted;
  let concluded = false;
  let schedulerFailure;
  const throwSchedulerFailure = () => {
    if (schedulerFailure !== void 0) throw schedulerFailure.error;
  };
  const commitReady = async () => {
    while (committed < group.length) {
      const slot = slots[committed];
      if (slot === void 0) break;
      const call = group[committed];
      const result = slot.needsPost ? await ctx.tools[TOOL_RUNTIME_SCHEDULER].finalize(slot.exec, slot.result) : ctx.tools[TOOL_RUNTIME_SCHEDULER].finish(slot.exec, slot.result);
      appendToolResult(session, turn, step, call.block, result, callSeqs[committed]);
      for (const context of result.additionalContexts ?? []) acceptContext(context);
      concluded ||= result.concludesTurn === true;
      committed++;
    }
  };
  const inFlight = /* @__PURE__ */ new Map();
  const startCall = async (index) => {
    const call = group[index];
    callSeqs[index] = appendToolCall(session, turn, step, call.block);
    started++;
    const prepared = await ctx.tools[TOOL_RUNTIME_SCHEDULER].prepare(call.exec);
    throwSchedulerFailure();
    switch (prepared.kind) {
      case "dispatch": {
        const promise = ctx.tools[TOOL_RUNTIME_SCHEDULER].dispatch(prepared.exec).then(
          (outcome) => {
            slots[index] = { exec: prepared.exec, result: outcome.result, needsPost: outcome.kind === "post-result" };
            return index;
          },
          (error) => {
            schedulerFailure ??= { error };
            return index;
          }
        );
        inFlight.set(index, promise);
        break;
      }
      case "post-result":
        slots[index] = { exec: prepared.exec, result: prepared.result, needsPost: true };
        break;
      case "final-result":
        slots[index] = { exec: prepared.exec, result: prepared.result, needsPost: false };
        break;
      /* v8 ignore next -- closed-union exhaustiveness guard */
      default:
        assertNever(prepared, "tool-call scheduler prepare result");
    }
  };
  const fillPool = async () => {
    while (!aborted && nextToStart < group.length && inFlight.size < maxParallelToolCalls) {
      const nextCall = group[nextToStart];
      if (nextToStart > 0 && mode === "parallel" && ctx.tools.executionMode(nextCall.exec).kind !== "parallel") break;
      await startCall(nextToStart);
      nextToStart++;
      throwSchedulerFailure();
      await commitReady();
      throwSchedulerFailure();
      if (signal.aborted) aborted = true;
    }
  };
  try {
    await fillPool();
    while (inFlight.size > 0) {
      const settledIndex = await Promise.race(inFlight.values());
      inFlight.delete(settledIndex);
      throwSchedulerFailure();
      await commitReady();
      throwSchedulerFailure();
      if (signal.aborted) aborted = true;
      await fillPool();
    }
  } catch (error) {
    schedulerFailure ??= { error };
    await Promise.allSettled(inFlight.values());
    throw schedulerFailure.error;
  }
  if (aborted) {
    for (const call of group.slice(started)) appendSkippedToolCall(session, turn, step, call.block);
    return { consumed: group.length, aborted: true, concluded };
  }
  if (committed !== started) throw new Error("tool-call scheduler: uncommitted settled calls");
  return { consumed: started, aborted: false, concluded };
}
function appendSkippedToolCall(session, turn, step, block) {
  const callSeq = appendToolCall(session, turn, step, block);
  appendToolResult(session, turn, step, block, {
    content: [{ type: "text", text: "Error: tool call aborted before dispatch" }],
    isError: true,
    error: {
      message: "tool call aborted before dispatch",
      info: { name: "AbortError", code: TOOL_ABORTED_BEFORE_DISPATCH }
    }
  }, callSeq);
}
function appendToolCall(session, turn, step, block) {
  const event = session.append("tool/call", { turn, step, callId: block.id, name: block.name, arguments: block.arguments });
  return event.seq;
}
function appendToolResult(session, turn, step, block, result, callSeq) {
  const message = createToolResultMessage({
    callId: block.id,
    content: result.content,
    isError: result.isError
  });
  session.append("tool/result", {
    turn,
    step,
    message,
    ...result.error?.info ? { error: result.error.info } : {},
    // The tool's private presentation payload (e.g. a result-time diff),
    // persisted so a UI bridge reproduces the card on replay.
    ...result.meta !== void 0 ? { meta: result.meta } : {}
  }, { surfaceOp: "append", sourceEventSeqs: [callSeq] });
}

// packages/core/agent-loop/src/agent.ts
function requestProposal(header) {
  if (header.adapterDefaults === void 0) return header.config;
  const proposal = { ...header.config };
  if (header.adapterDefaults.reasoningEffort === true) delete proposal.reasoningEffort;
  if (header.adapterDefaults.maxTokens === true) delete proposal.maxTokens;
  return proposal;
}
var ReactLoopAgent = class {
  constructor(loopCtx, id, options, session) {
    this.loopCtx = loopCtx;
    this.id = id;
    this.options = options;
    this.session = session;
    this.dispatch = agentEvents(loopCtx, this);
    this.inbox = new Inbox(session, {
      inserted: (message) => {
        this.dispatch.emit("agent/inbox/inserted", { message });
      },
      discarded: (message) => {
        this.dispatch.emit("agent/inbox/discarded", { message });
      },
      claimed: (message, turn) => {
        this.dispatch.emit("agent/inbox/claimed", { message, turn });
      }
    });
    const lastTurn = session.events.findLast((event) => event.type === "turn/start")?.data.turn ?? 0;
    this.phase = { kind: "idle", lastTurn };
    this.scope = createScope(loopCtx, this);
    this.ctx = this.scope.ctx.extend({ agent: this });
    this.runtimeContext = new RuntimeContextProjection(this.ctx, session);
  }
  loopCtx;
  id;
  options;
  session;
  inbox;
  phase;
  activityDone = Promise.resolve();
  /** The agent-scoped registration boundary; the lifecycle owner unwinds it after the driver exits. */
  scope;
  ctx;
  /** Fused dispatcher, built once in the constructor so hot-path dispatches never allocate. */
  dispatch;
  /** Whether this loop instance has appended its initial/resume request anchor. */
  requestHeaderLogged = false;
  runtimeContext;
  get status() {
    return this.phase.kind === "idle" || this.phase.kind === "maintenance" ? "idle" : "running";
  }
  /** Commit a phase and publish its externally visible status transition. */
  setPhase(next) {
    const previousStatus = this.status;
    this.phase = next;
    const status = this.status;
    if (status !== previousStatus) {
      this.dispatch.emit("agent/status", { status });
    }
  }
  send(message, target, wakeup) {
    const wakingAfterAbort = wakeup && this.phase.kind !== "idle" && this.phase.abort.signal.aborted;
    const resolvedTarget = wakingAfterAbort ? "next-turn" : target;
    this.inbox.splice(resolvedTarget, Infinity, 0, [message]);
    if (wakeup) this.wakeDriver(wakingAfterAbort);
  }
  followup(input) {
    this.send(input, "next-turn", true);
  }
  steer(input) {
    this.send(input, "next-step", true);
  }
  inject(input) {
    this.send(input, "next-step", false);
  }
  cancel(cause, options = {}) {
    if (!options.keepInbox) {
      this.inbox.clear();
      if (this.phase.kind !== "idle") this.phase.wakeRequested = false;
    }
    if (this.phase.kind !== "idle") this.phase.abort.abort(cause);
  }
  runMaintenance(job) {
    if (this.phase.kind !== "idle") throw new Error(`agent "${this.id}" already has active work`);
    const done = Promise.withResolvers();
    const maintenance = {
      kind: "maintenance",
      abort: new AbortController(),
      lastTurn: this.phase.lastTurn,
      wakeRequested: false
    };
    this.setPhase(maintenance);
    this.activityDone = done.promise;
    return (async () => {
      try {
        return await job(maintenance.abort.signal);
      } finally {
        this.setPhase({ kind: "idle", lastTurn: maintenance.lastTurn });
        if (maintenance.wakeRequested && this.inbox.hasPending) this.wakeDriver();
        done.resolve();
      }
    })();
  }
  /**
   * Start one driver, or latch its wake behind maintenance or an aborted
   * activity. A wake sent while idle always opens its turn boundary, even
   * when its message was cleared; only a latched replay is suppressed when
   * the queue no longer holds the wake.
   * @param wakeAfterAbort - the {@link send} classification, captured before
   *   the inbox insertion so a reentrant cancel cannot reclassify it.
   */
  wakeDriver(wakeAfterAbort = false) {
    if (this.phase.kind !== "idle") {
      const reason = this.phase.abort.signal.reason;
      if (reason?.kind !== "disposed" && (this.phase.kind === "maintenance" || wakeAfterAbort)) {
        this.phase.wakeRequested = true;
      }
      return;
    }
    const driver = Promise.withResolvers();
    this.activityDone = driver.promise;
    this.setPhase({
      kind: "running",
      abort: new AbortController(),
      turn: this.phase.lastTurn,
      step: 0,
      wakeRequested: false
    });
    this.loopCtx.agents.withInitiator(this, () => this.kick()).then(driver.resolve, driver.reject);
  }
  async whenIdle() {
    let activity;
    do {
      await (activity = this.activityDone);
    } while (activity !== this.activityDone);
  }
  /** Report one failure at its live boundary, then preserve it for driver containment. */
  throwError(error) {
    const turn = this.phase.kind === "running" ? this.phase.turn : this.phase.lastTurn;
    const step = this.phase.kind === "running" ? this.phase.step : 0;
    this.dispatch.emit("agent/error", { turn, step, error });
    throw error;
  }
  async kick() {
    try {
      while (await this.turn()) {
      }
    } catch (_error) {
    } finally {
      if (this.phase.kind === "running") {
        const { turn, wakeRequested } = this.phase;
        this.setPhase({ kind: "idle", lastTurn: turn });
        if (wakeRequested && this.inbox.hasPending) this.wakeDriver();
      }
    }
  }
  async preStep(target, position) {
    if (this.phase.kind !== "running") throw new Error(`agent "${this.id}": pre-step outside running phase`);
    const signal = this.phase.abort.signal;
    const claimed = this.inbox.claim(target, position.turn);
    const assembly = await this.loopCtx.systemPrompt.assemble(assembleContextFor(this, signal));
    signal.throwIfAborted();
    const sections = renderContextSections(assembly);
    const context = this.runtimeContext.project(joinContextSections(sections), sections);
    const decision = await this.dispatch.waterfall(
      "agent/pre-step",
      { messages: claimed, ...position, signal },
      () => Promise.resolve({
        kind: "enter",
        messages: context === void 0 ? claimed : [...claimed, context]
      })
    );
    signal.throwIfAborted();
    return decision.kind === "reject" ? decision : { ...decision, assembly };
  }
  /** Open one turn before claiming its first proposed step. */
  async turn() {
    if (this.phase.kind !== "running") {
      this.throwError(new Error(`agent "${this.id}": turn without driver reservation`));
    }
    const phase = this.phase;
    const { signal } = phase.abort;
    signal.throwIfAborted();
    const turn = phase.turn + 1;
    try {
      this.session.append("turn/start", { turn });
    } catch (error) {
      this.throwError(error);
    }
    phase.turn = turn;
    let turnEnds = null;
    let target = "next-turn";
    try {
      while (true) {
        signal.throwIfAborted();
        const step = phase.step + 1;
        const decision = await this.preStep(target, { turn, step });
        if (decision.kind === "reject") {
          turnEnds = { kind: "blocked" };
          return false;
        }
        if (turnEnds && decision.messages.length === 0) break;
        if (phase.step === 0 && decision.messages.length === 0) {
          turnEnds = { kind: "completed" };
          return false;
        }
        signal.throwIfAborted();
        this.session.append("step/start", { turn, step });
        phase.step = step;
        try {
          for (const message of decision.messages) {
            this.session.append("user/message", message, { surfaceOp: "append" });
          }
          const stepEnd = await this.step(decision.assembly);
          if (turnEnds === null || turnEnds.kind !== "max-tokens") turnEnds = stepEnd;
        } finally {
          this.session.append("step/end", { turn, step });
        }
        signal.throwIfAborted();
        if (turnEnds && this.inbox.nextStep.length === 0) {
          await this.dispatch.serial("agent/turn-stopping", { turn, signal });
          signal.throwIfAborted();
        }
        if (turnEnds && this.inbox.nextStep.length === 0) break;
        target = "next-step";
      }
    } catch (error) {
      if (signal.aborted) {
        turnEnds = { kind: "aborted", reason: signal.reason };
        throw error;
      }
      turnEnds = {
        kind: "error",
        error: error instanceof LlmError ? error.failure : { message: errorChain(error), code: "UNKNOWN" }
      };
      this.throwError(error);
    } finally {
      try {
        this.session.append("turn/end", { turn, reason: turnEnds });
      } catch (error) {
        this.throwError(error);
      }
    }
    if (!this.inbox.hasPending) return false;
    phase.abort = new AbortController();
    phase.wakeRequested = false;
    phase.step = 0;
    return true;
  }
  async step(assembly) {
    if (this.phase.kind !== "running") throw new Error(`agent "${this.id}": step outside running phase`);
    const { turn, step, abort: { signal } } = this.phase;
    signal.throwIfAborted();
    const system = renderPrompt(assembly);
    while (true) {
      const { request, preparedCall } = await this.buildRequest(
        turn,
        step,
        assembly.tools,
        system,
        this.session.deriveMessages(),
        signal
      );
      const assembler = new BlockAssembler();
      const chunkSeqs = [];
      const stream = preparedCall?.stream(request) ?? this.loopCtx.llm.stream(request);
      signal.throwIfAborted();
      for await (const chunk of stream) {
        signal.throwIfAborted();
        chunkSeqs.push(this.session.append("assistant/chunk", { turn, step, chunk }).seq);
        assembler.push(chunk);
      }
      signal.throwIfAborted();
      const finish = assembler.finish;
      if (finish.kind === "error" || finish.kind === "aborted") {
        const action = await this.dispatch.waterfall(
          "agent/request-error",
          {
            turn,
            step,
            provider: request.provider,
            failure: finish.failure,
            retryPolicy: preparedCall?.retryPolicy,
            signal
          },
          () => Promise.resolve(void 0)
        );
        signal.throwIfAborted();
        if (action?.kind !== "retry") {
          throw new LlmError(finish.failure.message, finish.failure.code, finish.failure);
        }
        continue;
      }
      const message = createAssistantMessage({
        content: assembler.blocks(),
        source: {
          provider: request.provider,
          model: request.model,
          ...assembler.replayState !== void 0 ? { replayState: assembler.replayState } : {}
        }
      });
      this.session.append(
        "assistant/message",
        {
          turn,
          step,
          message,
          ...assembler.usage === void 0 ? {} : { usage: assembler.usage }
        },
        { surfaceOp: "append", sourceEventSeqs: chunkSeqs }
      );
      if (finish.kind === "max-tokens") return { kind: "max-tokens" };
      const toolCalls = message.content.filter((block) => block.type === "tool-call");
      if (toolCalls.length === 0) return { kind: "completed" };
      const { concluded } = await executeToolCalls(
        this.loopCtx,
        turn,
        step,
        toolCalls,
        signal,
        (context) => this.inbox.splice("next-step", this.inbox.nextStep.length, 0, [context])
      );
      return concluded ? { kind: "completed" } : null;
    }
  }
  /**
   * Compose one frozen request and bind it to the adapter registration that
   * resolved its exact-model defaults.
   */
  async buildRequest(turn, step, tools, system, boundaryMessages, signal) {
    const { session } = this;
    const persistedHeader = session.requestHeader();
    const persistedConfig = persistedHeader?.config;
    const route = { provider: this.options.provider ?? "", model: this.options.model ?? "" };
    const reasoningEffort = persistedConfig?.provider === route.provider && persistedConfig.model === route.model && persistedHeader?.adapterDefaults?.reasoningEffort !== true ? persistedConfig.reasoningEffort : void 0;
    const maxTokens = this.options.maxTokens;
    const seedConfig = deepFreeze(structuredClone(
      this.requestHeaderLogged ? requestProposal(persistedHeader) : {
        ...route,
        ...reasoningEffort === void 0 ? {} : { reasoningEffort },
        ...maxTokens === void 0 ? {} : { maxTokens }
      }
    ));
    const proposedConfig = await this.dispatch.waterfall(
      "agent/request",
      { turn, step, signal },
      () => Promise.resolve(seedConfig)
    );
    signal.throwIfAborted();
    if (!proposedConfig.provider || !proposedConfig.model) {
      throw new Error(`agent "${this.id}" has no provider/model: set AgentOptions.provider and AgentOptions.model or supply both via the agent/request waterfall`);
    }
    let config;
    let preparedCall;
    try {
      preparedCall = await this.loopCtx.llm.prepareCall(proposedConfig, signal);
      config = preparedCall.config;
    } catch (error) {
      if (!(error instanceof LlmError) || error.code !== "NO_ADAPTER") throw error;
      config = proposedConfig;
    }
    signal.throwIfAborted();
    const header = canonicalHeader({
      config,
      ...preparedCall === void 0 ? {} : { adapterDefaults: preparedCall.adapterDefaults },
      ...system ? { system } : {},
      ...tools.length > 0 ? { tools } : {}
    });
    const baseline = this.session.requestHeader();
    if (!this.requestHeaderLogged) {
      this.session.append("request/header", { header, reason: baseline === void 0 ? "initial" : "resume" });
      this.requestHeaderLogged = true;
    } else if (baseline === void 0 || !headerEquals(baseline, header)) {
      this.session.append("request/header", { header, reason: "change" });
    }
    const contextWindow = preparedCall?.context?.contextWindow;
    const requestContext = {
      provider: config.provider,
      model: config.model,
      ...contextWindow === void 0 ? {} : { contextWindow }
    };
    const previousContext = session.requestContext();
    if (previousContext?.provider !== requestContext.provider || previousContext.model !== requestContext.model || previousContext.contextWindow !== requestContext.contextWindow) {
      session.append("request/context", requestContext);
    }
    signal.throwIfAborted();
    const request = markAgentLoopRequest(deepFreeze({
      ...header.config,
      messages: boundaryMessages,
      ...header.system !== void 0 ? { system: header.system } : {},
      ...header.tools !== void 0 ? { tools: header.tools } : {},
      sessionId: this.session.id,
      signal
    }));
    return { request, ...preparedCall === void 0 ? {} : { preparedCall } };
  }
};

// packages/core/agent-loop/src/constants.ts
var DEFAULT_MAX_PARALLEL_TOOL_CALLS = 10;

// packages/core/agent-loop/src/index.ts
var INACTIVE_STATES = /* @__PURE__ */ new Set([
  FiberState.UNLOADING,
  FiberState.DISPOSED,
  FiberState.FAILED
]);
var FactoryOwnership = class {
  constructor(fiber) {
    this.fiber = fiber;
  }
  fiber;
  accepting = true;
  teardown = new AbortController();
  inactive = Promise.withResolvers();
  liveAgents = /* @__PURE__ */ new Set();
  startupTasks = /* @__PURE__ */ new Set();
  /** Aborts (reason: `agent loop is not active` error) when factory teardown begins. */
  get signal() {
    return this.teardown.signal;
  }
  isActive() {
    return this.accepting && !INACTIVE_STATES.has(this.fiber.state);
  }
  /** Track one live agent's shared teardown until it has run. */
  track(dispose) {
    this.liveAgents.add(dispose);
    return () => {
      this.liveAgents.delete(dispose);
    };
  }
  /** Join config startup work that begins before an agent exists. */
  trackStartup(job) {
    this.startupTasks.add(job);
    const forget = () => {
      this.startupTasks.delete(job);
    };
    void job.then(forget, forget);
  }
  /** Join one public create/resume continuation; factory dispose awaits its settlement. */
  trackWrapper(job) {
    this.trackStartup(job.then(() => void 0, () => void 0));
  }
  /** Resolve `task`, or stop waiting when factory teardown begins. */
  async waitWhileActive(job) {
    await Promise.race([job, this.inactive.promise]);
  }
  async dispose() {
    this.accepting = false;
    this.teardown.abort(new Error("agent loop is not active"));
    this.inactive.resolve();
    await Promise.all([
      ...[...this.liveAgents].map((dispose) => dispose()),
      ...this.startupTasks
    ]);
  }
};
async function raceAbort(operation, signal, id) {
  const toAbortError = () => signal.reason instanceof Error ? signal.reason : new Error(`agent "${id}" creation aborted`, { cause: signal.reason });
  if (signal.aborted) throw toAbortError();
  const aborted = Promise.withResolvers();
  const listener = () => {
    aborted.reject(toAbortError());
  };
  signal.addEventListener("abort", listener, { once: true });
  try {
    return await Promise.race([Promise.resolve(operation), aborted.promise]);
  } finally {
    signal.removeEventListener("abort", listener);
  }
}
async function raceAbortCall(operation, signal, id, releaseAbandoned) {
  if (signal.aborted) {
    throw signal.reason instanceof Error ? signal.reason : new Error(`agent "${id}" creation aborted`, { cause: signal.reason });
  }
  const pending = Promise.resolve().then(operation);
  try {
    return await raceAbort(pending, signal, id);
  } catch (error) {
    if (signal.aborted && releaseAbandoned !== void 0) {
      void pending.then(releaseAbandoned, () => void 0);
    }
    throw error;
  }
}
function resolveMaxParallelToolCalls(value) {
  const maxParallelToolCalls = value ?? DEFAULT_MAX_PARALLEL_TOOL_CALLS;
  if (!Number.isInteger(maxParallelToolCalls) || maxParallelToolCalls < 1) {
    throw new Error("maxParallelToolCalls must be a positive integer");
  }
  return maxParallelToolCalls;
}
function assertAgentOptions(options) {
  if (options.maxTokens !== void 0 && (!Number.isSafeInteger(options.maxTokens) || options.maxTokens <= 0)) {
    throw new TypeError("agent maxTokens must be a positive safe integer");
  }
}
var CONFIGURED_AGENT_IDENTITIES_KEY = "configuredAgentIdentities";
function applyLauncherIdentities(agents, identities) {
  if (identities === void 0) return agents;
  return agents.map((agent) => {
    const identity = identities[agent.id];
    if (identity === void 0) return agent;
    const { sessionId: _sessionId, resumeSessionId: _resumeSessionId, ...rest } = agent;
    return identity.resume ? { ...rest, resumeSessionId: identity.id } : { ...rest, sessionId: identity.id };
  });
}
var AGENT_LOOP_SETTINGS_NAMESPACE = settingsNamespace("agent-loop");
var AGENT_LOOP_SETTINGS_SCHEMA = z.object({
  maxParallelToolCalls: z.number().step(1).min(1).default(DEFAULT_MAX_PARALLEL_TOOL_CALLS)
});
function validateConfiguredAgents(agents) {
  const exactIdentities = /* @__PURE__ */ new Map();
  for (const { id, sessionId, resumeSessionId } of agents) {
    const hasResumeId = resumeSessionId !== void 0 && resumeSessionId !== "";
    if (sessionId !== void 0 && hasResumeId) {
      throw new Error(`agent "${id}": sessionId and resumeSessionId are mutually exclusive`);
    }
    const exactIdentity = hasResumeId ? resumeSessionId : sessionId;
    if (exactIdentity === void 0) continue;
    const firstId = exactIdentities.get(exactIdentity);
    if (firstId !== void 0) {
      throw new Error(`agents "${firstId}" and "${id}" use duplicate exact session identity "${exactIdentity}"`);
    }
    exactIdentities.set(exactIdentity, id);
  }
}
var AgentLoop = class extends Service {
  static inject = ["agents", "sessions", "llm", "tools", "systemPrompt"];
  /** Runtime schema for declarative agents. */
  static Config = z.object({
    maxParallelToolCalls: z.number().step(1).min(1).default(DEFAULT_MAX_PARALLEL_TOOL_CALLS),
    agents: z.array(z.object({
      id: z.string().required(),
      sessionId: z.string().min(1),
      provider: z.string(),
      model: z.string(),
      maxTokens: z.number().step(1).min(1).max(Number.MAX_SAFE_INTEGER),
      cwd: z.string(),
      resumeSessionId: z.string()
    })).default([])
  });
  /** Validated configuration owned by the agent-loop service. */
  config;
  ownership;
  /** Plain holder prevents Cordis from re-tracing the factory's dependency context through a caller shadow. */
  runtime;
  constructor(ctx, config) {
    super(ctx, "agentLoop");
    const entry = {
      maxParallelToolCalls: resolveMaxParallelToolCalls(config.maxParallelToolCalls)
    };
    let source = () => entry;
    this.config = {
      ...config,
      agents: applyLauncherIdentities(config.agents, ctx.get(CONFIGURED_AGENT_IDENTITIES_KEY)),
      // Read through on every scheduler decision: `tool-calls.ts` destructures
      // this at the start of each group, so a committed change caps the next
      // group without disturbing the one in flight.
      get maxParallelToolCalls() {
        return source().maxParallelToolCalls;
      }
    };
    installSettingsSection(ctx, AGENT_LOOP_SETTINGS_NAMESPACE, AGENT_LOOP_SETTINGS_SCHEMA, entry, {
      // The schema admits any integer above zero; `resolveMaxParallelToolCalls`
      // owns the whole rule, so refusing here keeps the running scheduler on
      // its last good cap instead of failing at the next tool group.
      validate: (value) => void resolveMaxParallelToolCalls(value.maxParallelToolCalls),
      setSource: (current) => {
        source = current;
      },
      // Nothing is derived from the cap: the getter above is the only reader.
      onChange: () => {
      }
    });
    validateConfiguredAgents(this.config.agents);
    this.ownership = new FactoryOwnership(ctx.fiber);
    this.runtime = { ctx };
    ctx.effect(() => () => this.ownership.dispose(), "agentLoop.transactions()");
    ctx.effect(() => ctx.agents.setFactory(this), "agentLoop.setFactory()");
    ctx.systemPrompt.variable("provider", (context) => context.agent?.options.provider);
    ctx.systemPrompt.variable("model", (context) => context.agent?.options.model);
    ctx.systemPrompt.variable("cwd", (context) => context.agent?.session.header.cwd);
    for (const { id, sessionId, cwd, resumeSessionId, ...options } of this.config.agents) {
      const meta = cwd === void 0 ? {} : { cwd };
      if (resumeSessionId === void 0 || resumeSessionId === "") {
        const configuredId = sessionId ?? SessionId(`${id}-session-${randomUUID()}`);
        const persistence = sessionId === void 0 ? void 0 : ctx.get("sessionPersistence");
        if (persistence === void 0) {
          this.create(configuredId, options, meta);
        } else {
          const startup = this.restoreOrCreateConfigured(ctx, persistence, configuredId, options, meta).catch((error) => {
            this.reportConfiguredStartupFailure(id, "restore", configuredId, error);
          });
          this.ownership.trackStartup(startup);
        }
        continue;
      }
      ctx.effect(() => {
        const fiber = ctx.inject(["sessionPersistence"], (childCtx) => {
          void this.resumeWith(ctx, childCtx.sessionPersistence, {
            resumeSessionId,
            agentOptions: options
          }).catch((error) => {
            this.reportConfiguredStartupFailure(id, "resume", resumeSessionId, error);
          });
        });
        return fiber.dispose;
      }, `agentLoop.resume(${id})`);
    }
  }
  /** Report a contained declarative-start failure to identity-bound consumers. */
  reportConfiguredStartupFailure(configId, action, sessionId, error) {
    if (!this.ownership.isActive()) return;
    this.ctx.logger.warn(`agent "${configId}": config-driven ${action} of "${sessionId}" failed: ${errorChain2(error)}`);
    const args = ["agent-loop/config-start-failed", { sessionId, error }];
    for (const callback of this.ctx.events.dispatch("emit", args)) {
      try {
        const returned = callback(...args);
        void Promise.resolve(returned).catch((listenerError) => {
          this.ctx.logger.warn(`agent "${configId}": config-start-failed listener rejected: ${errorChain2(listenerError)}`);
        });
      } catch (listenerError) {
        this.ctx.logger.warn(`agent "${configId}": config-start-failed listener threw: ${errorChain2(listenerError)}`);
      }
    }
  }
  /** Restore a materialized exact config identity on remount, or create it on first use. */
  async restoreOrCreateConfigured(ownerCtx, persistence, sessionId, agentOptions, meta) {
    await this.waitForDrainingConfiguredIdentity(ownerCtx, sessionId);
    if (!this.ownership.isActive()) return;
    try {
      await this.resumeWith(ownerCtx, persistence, { resumeSessionId: sessionId, agentOptions });
      return;
    } catch (error) {
      if (!this.ownership.isActive()) return;
      const exists = (await persistence.list()).some((header) => header.id === sessionId);
      if (exists) throw error;
    }
    this.create(sessionId, agentOptions, meta);
  }
  /** Wait for a draining same-id lifecycle to finish registry teardown. */
  async waitForDrainingConfiguredIdentity(ownerCtx, sessionId) {
    if (ownerCtx.agents.get(sessionId) === void 0 && ownerCtx.sessions.get(sessionId) === void 0) return;
    const released = Promise.withResolvers();
    const checkReleased = () => {
      if (ownerCtx.agents.get(sessionId) === void 0 && ownerCtx.sessions.get(sessionId) === void 0) {
        released.resolve();
      }
    };
    const disposeAgentListener = ownerCtx.on("agent/disposed", () => {
      checkReleased();
    });
    const disposeSessionListener = ownerCtx.on("session/disposed", checkReleased);
    try {
      checkReleased();
      await this.ownership.waitWhileActive(released.promise);
    } finally {
      disposeAgentListener();
      disposeSessionListener();
    }
  }
  /**
   * Construct the driver, scope, and one memoized reverse teardown for a new
   * agent. The teardown is registered with the factory and the owner fiber
   * BEFORE publication, so a mid-setup unload rolls everything back; `signal`
   * fuses caller cancellation with lifecycle teardown for setup awaits.
   */
  prepare(ownerCtx, id, options, session, callerSignal) {
    assertAgentOptions(options);
    ownerCtx.fiber.assertActive();
    if (!this.ownership.isActive()) throw new Error("agent loop is not active");
    if (callerSignal?.aborted) {
      throw callerSignal.reason instanceof Error ? callerSignal.reason : new Error(`agent "${id}" creation aborted`, { cause: callerSignal.reason });
    }
    const loopCtx = this.runtime.ctx;
    const abort = new AbortController();
    const onCallerAbort = () => {
      abort.abort(callerSignal?.reason instanceof Error ? callerSignal.reason : new Error(`agent "${id}" creation aborted`, { cause: callerSignal?.reason }));
    };
    const onFactoryTeardown = () => {
      abort.abort(this.ownership.signal.reason);
    };
    callerSignal?.addEventListener("abort", onCallerAbort, { once: true });
    this.ownership.signal.addEventListener("abort", onFactoryTeardown, { once: true });
    let machine;
    let detachSession;
    let detachAgent;
    let disposing;
    const machineReady = Promise.withResolvers();
    const dispose = (ownerTriggered = false) => disposing ??= (async () => {
      abort.abort(new Error(`agent "${id}" lifecycle disposed`));
      callerSignal?.removeEventListener("abort", onCallerAbort);
      this.ownership.signal.removeEventListener("abort", onFactoryTeardown);
      try {
        if (machine === void 0) await machineReady.promise;
        if (machine !== void 0) {
          machine.cancel({ kind: "disposed" });
          await machine.whenIdle();
          await machine.scope.dispose();
        }
      } finally {
        try {
          detachAgent?.();
          detachSession?.();
        } finally {
          untrack();
          if (!ownerTriggered) await unfollowOwner();
        }
      }
    })();
    const untrack = this.ownership.track(dispose);
    let unfollowOwner;
    try {
      unfollowOwner = ownerCtx.effect(() => () => {
        if (disposing !== void 0) return;
        abort.abort(new Error(`agent "${id}" setup aborted: owner disposed during setup`));
        return dispose(true);
      }, `agentLoop.lifecycle(${id})`);
    } catch (error) {
      untrack();
      callerSignal?.removeEventListener("abort", onCallerAbort);
      this.ownership.signal.removeEventListener("abort", onFactoryTeardown);
      throw error;
    }
    const assertLive = () => {
      if (!abort.signal.aborted) return;
      throw abort.signal.reason instanceof Error ? abort.signal.reason : new Error(String(abort.signal.reason));
    };
    try {
      const agent = machine = new ReactLoopAgent(loopCtx, id, options, session);
      machineReady.resolve();
      assertLive();
      return {
        agent,
        signal: abort.signal,
        publish: (source) => {
          assertLive();
          detachSession = agent.ctx.sessions.enter(session);
          detachAgent = loopCtx.agents.enter(agent, ownerCtx.agent);
          agent.ctx.sessions.announce(session);
          assertLive();
          loopCtx.agents.announce(agent);
          assertLive();
          emitAgentEvent(loopCtx, agent, "agent/session-start", { source });
          assertLive();
          return { agent, dispose };
        },
        dispose
      };
    } catch (error) {
      machineReady.resolve();
      void dispose();
      throw error;
    }
  }
  /**
   * Create an agent and session under one caller-supplied identity, owned by
   * the accessing fiber. Constructor-driven config calls mint a fresh combined
   * id before entering this boundary.
   * @param id - shared agent/session identity.
   * @param options - concrete loop options.
   * @param meta - optional fresh-session workspace metadata.
   * @returns the published running agent.
   */
  create(id, options = {}, meta = {}) {
    var _stack = [];
    try {
      const preparation = __using(_stack, SessionPreparation.create(this.runtime.ctx.sessions.prepare(id, { meta })));
      const prepared = this.prepare(this.ctx, id, options, preparation.session);
      try {
        return prepared.publish("startup").agent;
      } catch (error) {
        void prepared.dispose();
        throw error;
      }
    } catch (_) {
      var _error = _, _hasError = true;
    } finally {
      __callDispose(_stack, _error, _hasError);
    }
  }
  /**
   * Create an owned agent on a caller-supplied session id.
   * @param ownerCtx - caller context that structurally owns the lifecycle.
   * @param options - identities, session seed/metadata, loop options, setup, and cancellation.
   * @returns the published handle.
   */
  async createAgent(ownerCtx, options) {
    const preparation = SessionPreparation.create(this.runtime.ctx.sessions.prepare(options.sessionId, {
      ...options.seed === void 0 ? {} : { seed: options.seed },
      ...options.meta === void 0 ? {} : { meta: options.meta }
    }));
    const published = this.setupAndPublish(
      ownerCtx,
      options.sessionId,
      preparation,
      options.agentOptions ?? {},
      options.setup,
      options.signal,
      "startup"
    );
    this.ownership.trackWrapper(published);
    return published;
  }
  /** Prepare one Agent around an acquired Session, run setup, and publish it. */
  async setupAndPublish(ownerCtx, id, preparation, agentOptions, setup, signal, source) {
    var _stack = [];
    try {
      const ownedPreparation = __using(_stack, preparation);
      const session = ownedPreparation.session;
      const prepared = this.prepare(ownerCtx, id, agentOptions, session, signal);
      try {
        const setupCommit = await raceAbort(setup?.(prepared.agent.ctx), prepared.signal, id);
        setupCommit?.commit();
        return prepared.publish(source);
      } catch (error) {
        await prepared.dispose();
        throw error;
      }
    } catch (_) {
      var _error = _, _hasError = true;
    } finally {
      __callDispose(_stack, _error, _hasError);
    }
  }
  /**
   * Resume an owned agent from the configured persistence service.
   * @param ownerCtx - caller context that owns load, setup, and the live lifecycle.
   * @param options - persisted identity, loop options, setup, and cancellation.
   * @returns the published handle.
   */
  async resume(ownerCtx, options) {
    const persistence = this.runtime.ctx.get("sessionPersistence");
    if (persistence === void 0) {
      throw new Error("cannot resume: session persistence is not configured (load a dsh-session-persistence backend)");
    }
    return this.resumeWith(ownerCtx, persistence, options);
  }
  /** Resume through an explicit persistence handle used by the deferred config path. */
  resumeWith(ownerCtx, persistence, options) {
    const id = options.resumeSessionId;
    const published = (async () => {
      const ownerAbort = new AbortController();
      const unfollowOwner = ownerCtx.effect(() => () => {
        ownerAbort.abort(new Error(`agent "${id}" setup aborted: owner disposed during setup`));
      }, `agentLoop.resume-load(${id})`);
      const fused = AbortSignal.any([
        ...options.signal === void 0 ? [] : [options.signal],
        ownerAbort.signal,
        this.ownership.signal
      ]);
      let preparation;
      try {
        try {
          preparation = await raceAbortCall(
            () => persistence.prepare(id, fused),
            fused,
            id,
            (abandoned) => {
              abandoned[Symbol.dispose]();
            }
          );
        } finally {
          await unfollowOwner();
        }
        ownerCtx.fiber.assertActive();
        if (!this.ownership.isActive()) throw new Error("agent loop is not active");
        return await this.setupAndPublish(
          ownerCtx,
          id,
          preparation,
          options.agentOptions ?? {},
          options.setup,
          options.signal,
          "resume"
        );
      } finally {
        preparation?.[Symbol.dispose]();
      }
    })();
    this.ownership.trackWrapper(published);
    return published;
  }
};
var index_default = AgentLoop;
export {
  AGENT_LOOP_SETTINGS_NAMESPACE,
  AGENT_LOOP_SETTINGS_SCHEMA,
  AgentLoop,
  CONFIGURED_AGENT_IDENTITIES_KEY,
  DEFAULT_MAX_PARALLEL_TOOL_CALLS,
  index_default as default
};
