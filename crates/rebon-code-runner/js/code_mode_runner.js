"use strict";

// One-shot Code Mode executor. This helper is compiled into the Rust host with include_str! and
// is intentionally unrelated to the plugin host: no package imports, shared framing, lifecycle,
// discovery, or plugin state.
const vm = require("node:vm");
const readline = require("node:readline");

const PROTOCOL_VERSION = 1;
const MIN_NODE_MAJOR = 20;
const MAX_TOOL_CALLS = 2048;

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
let initialResolve;
let initialReject;
let receivedInitial = false;
const initial = new Promise((resolve, reject) => {
  initialResolve = resolve;
  initialReject = reject;
});
const pending = new Map();

// The cap on what this side may send, taken from the execute frame rather than
// declared here: Rust owns the number, and a second copy could disagree with it.
// Until the frame arrives there is nothing to send, so `undefined` cannot be
// reached by `send`.
let maxFrameBytes;
// What this side will read before it has been told the real cap. It guards the
// handshake only: the execute frame is the first line, and Rust bounds that one
// with its own frame limit. Once the frame arrives this is replaced by the
// number Rust sent, which is larger because a tool result is the one thing that
// legitimately gets big.
let maxIncomingBytes = 2 * 1024 * 1024;

function send(frame) {
  const line = JSON.stringify(frame);
  if (Buffer.byteLength(line, "utf8") + 1 > maxFrameBytes) {
    throw new Error("executor frame exceeds the hard limit");
  }
  process.stdout.write(`${line}\n`);
}

function protocolError(message) {
  send({ v: PROTOCOL_VERSION, type: "terminal", ok: false, kind: "protocol", message, logs: [] });
  process.exitCode = 65;
}

rl.on("line", (line) => {
  if (Buffer.byteLength(line, "utf8") + 1 > maxIncomingBytes) {
    if (!receivedInitial) initialReject(new Error("request frame exceeds the hard limit"));
    return;
  }
  let frame;
  try {
    frame = JSON.parse(line);
  } catch {
    if (!receivedInitial) initialReject(new Error("request is not strict JSON"));
    return;
  }
  if (!receivedInitial) {
    receivedInitial = true;
    initialResolve(frame);
    return;
  }
  if (!frame || frame.v !== PROTOCOL_VERSION || frame.type !== "tool_result" ||
      !Number.isSafeInteger(frame.id) || frame.id < 0 || typeof frame.ok !== "boolean") {
    return;
  }
  const waiter = pending.get(frame.id);
  if (!waiter) return;
  pending.delete(frame.id);
  if (frame.ok) waiter.resolve(frame.value);
  else waiter.reject(new Error(typeof frame.error === "string" ? frame.error : "tool dispatch failed"));
});
rl.on("error", (error) => initialReject(error));

function jsonClone(value, label, allowUndefined) {
  if (value === undefined && allowUndefined) return { undefined: true };
  let encoded;
  try {
    encoded = JSON.stringify(value, (_key, current) => {
      const kind = typeof current;
      if ((kind === "number" && !Number.isFinite(current)) ||
          kind === "bigint" || kind === "function" || kind === "symbol" ||
          current === undefined) {
        throw new Error(`unsupported ${kind} value`);
      }
      return current;
    });
  } catch (error) {
    throw new Error(`${label} is not lossless JSON: ${error && error.message ? error.message : String(error)}`);
  }
  if (encoded === undefined) {
    throw new Error(`${label} is not lossless JSON`);
  }
  return { value: JSON.parse(encoded) };
}

function errorMessage(error) {
  if (error && typeof error.message === "string") return error.message;
  try { return String(error); } catch { return "unknown JavaScript error"; }
}

(async () => {
  const request = await initial;
  const nodeMajor = Number(process.versions.node.split(".", 1)[0]);
  if (!Number.isSafeInteger(nodeMajor) || nodeMajor < MIN_NODE_MAJOR) {
    protocolError(`Code Mode requires Node ${MIN_NODE_MAJOR}+; found ${process.versions.node}`);
    return;
  }
  if (!request || request.v !== PROTOCOL_VERSION || request.type !== "execute" ||
      typeof request.code !== "string" || !request.limits ||
      !Number.isSafeInteger(request.limits.maxLogBytes) || request.limits.maxLogBytes < 0 ||
      !Number.isSafeInteger(request.limits.maxLogLines) || request.limits.maxLogLines < 0 ||
      !Number.isSafeInteger(request.limits.maxOutputBytes) || request.limits.maxOutputBytes < 0 ||
      !Number.isSafeInteger(request.limits.maxFrameBytes) || request.limits.maxFrameBytes <= 0 ||
      !Number.isSafeInteger(request.limits.maxIncomingBytes) || request.limits.maxIncomingBytes <= 0) {
    protocolError("invalid execute request or protocol version mismatch");
    return;
  }
  maxFrameBytes = request.limits.maxFrameBytes;
  maxIncomingBytes = request.limits.maxIncomingBytes;

  const logs = [];
  let logBytes = 0;
  let calls = 0;
  let nextId = 0;
  const render = (value) => {
    if (typeof value === "string") return value;
    try {
      const encoded = JSON.stringify(value);
      return encoded === undefined ? String(value) : encoded;
    } catch {
      return String(value);
    }
  };
  const log = (...args) => {
    const line = args.map(render).join(" ");
    const bytes = Buffer.byteLength(line, "utf8");
    if (logs.length >= request.limits.maxLogLines ||
        logBytes + bytes > request.limits.maxLogBytes) {
      throw new Error("Code Mode log output exceeds the hard limit");
    }
    logs.push(line);
    logBytes += bytes;
  };
  const invoke = (name, input) => {
    if (++calls > MAX_TOOL_CALLS) {
      return Promise.reject(new Error("Code Mode tool-call count exceeds the hard limit"));
    }
    const id = nextId++;
    let cloned;
    try {
      cloned = jsonClone(input === undefined || input === null ? {} : input, "tool input", false).value;
    } catch (error) {
      return Promise.reject(error);
    }
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      try {
        send({ v: PROTOCOL_VERSION, type: "tool_call", id, name: String(name), input: cloned });
      } catch (error) {
        pending.delete(id);
        reject(error);
      }
    });
  };

  const sandbox = Object.create(null);
  const context = vm.createContext(sandbox, {
    name: "rebon-code-mode",
    codeGeneration: { strings: false, wasm: false },
  });
  // Never put a host-realm function, Promise, Error, or object directly in the sandbox. Host
  // functions expose their own Function constructor through `.constructor`, bypassing the VM's
  // string-code-generation ban. This bootstrap runs in the sandbox realm and closes over the two
  // host callbacks. Tool results are JSON-cloned again inside that realm before becoming visible.
  const createFacades = new vm.Script(`
    ((hostInvoke, hostLog) => {
      const SafePromise = Promise;
      const SafeError = Error;
      const safeResolve = SafePromise.resolve.bind(SafePromise);
      const safeThen = Function.call.bind(SafePromise.prototype.then);
      const safeStringify = JSON.stringify.bind(JSON);
      const safeParse = JSON.parse.bind(JSON);
      const safeHas = Reflect.has.bind(Reflect);
      const safeString = String;
      const toSafeError = (error) => new SafeError(
        error && typeof error.message === "string" ? error.message : safeString(error)
      );
      const invoke = (name, input) => new SafePromise((resolve, reject) => {
        const dispatched = safeThen(safeResolve(), () => hostInvoke(safeString(name), input));
        safeThen(
          dispatched,
          (value) => resolve(safeParse(safeStringify(value))),
          (error) => reject(toSafeError(error)),
        );
      });
      const log = (...args) => {
        try { hostLog(...args); }
        catch (error) { throw toSafeError(error); }
      };
      const consoleFacade = Object.freeze({
        log, info: log, warn: log, error: log, debug: log, trace: log,
      });
      const toolsTarget = Object.freeze({ invoke });
      const tools = new Proxy(toolsTarget, {
        get(target, name) {
          if (safeHas(target, name)) return target[name];
          if (typeof name === "symbol") return undefined;
          return (input) => invoke(safeString(name), input);
        },
        set() { return false; },
      });
      return Object.freeze({
        console: consoleFacade,
        tools,
        createError: (message) => new SafeError(safeString(message)),
      });
    })
  `).runInContext(context);
  const facades = createFacades(invoke, log);
  Object.defineProperties(sandbox, {
    console: { value: facades.console, enumerable: true },
    tools: { value: facades.tools, enumerable: true },
  });

  try {
    const script = new vm.Script(`(async () => {\n${request.code}\n})()`, {
      filename: "rebon:run_code",
      importModuleDynamically() {
        throw facades.createError("dynamic import is disabled in Code Mode");
      },
    });
    const result = await script.runInContext(context, { breakOnSigint: false });
    const cloned = jsonClone(result, "program return value", true);
    const terminal = {
      v: PROTOCOL_VERSION,
      type: "terminal",
      ok: true,
      resultUndefined: cloned.undefined === true,
      result: cloned.value,
      logs,
    };
    const encoded = JSON.stringify(terminal);
    if (Buffer.byteLength(encoded, "utf8") > request.limits.maxOutputBytes) {
      send({
        v: PROTOCOL_VERSION,
        type: "terminal",
        ok: false,
        kind: "output_limit",
        message: "Code Mode result exceeds the hard output limit",
        logs: [],
      });
    } else {
      send(terminal);
    }
  } catch (error) {
    const terminal = {
      v: PROTOCOL_VERSION,
      type: "terminal",
      ok: false,
      kind: error && error.name === "SyntaxError" ? "syntax" : "runtime",
      message: errorMessage(error),
      logs,
    };
    const encoded = JSON.stringify(terminal);
    if (Buffer.byteLength(encoded, "utf8") > request.limits.maxOutputBytes) {
      send({
        v: PROTOCOL_VERSION,
        type: "terminal",
        ok: false,
        kind: "output_limit",
        message: "Code Mode error output exceeds the hard output limit",
        logs: [],
      });
    } else {
      send(terminal);
    }
    process.exitCode = 1;
  } finally {
    rl.close();
  }
})().catch((error) => {
  try {
    protocolError(errorMessage(error));
  } catch {
    process.exitCode = 70;
  }
});
