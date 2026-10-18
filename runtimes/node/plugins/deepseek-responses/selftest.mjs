#!/usr/bin/env node
// Offline self-test for the deepseek-responses provider plugin.
//
//   node runtimes/node/plugins/deepseek-responses/selftest.mjs
//
// Part 1 unit-tests the request/stream translation layers in-process.
// Part 2 drives the plugin the way rebon's plugin host does — `activate`, one
// registered adapter, one turn per call with an `emit` and an abort signal —
// against a local mock DeepSeek SSE server. No network, and no child process:
// the provider runs on the shared plugin host, not one of its own.

import { createServer } from "node:http";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  activate,
  applyRequestOptions,
  buildInput,
  buildRequestBody,
  errorTypeForStatus,
  ResponseEventTranslator,
  stringifyJson,
  translateReasoningEffort,
  translateUsage,
  withTransientContext,
} from "./provider.mjs";

let failures = 0;
let checks = 0;

function assert(condition, label) {
  checks += 1;
  if (!condition) {
    failures += 1;
    console.error(`FAIL: ${label}`);
  }
}

function assertEq(actual, expected, label) {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  assert(a === b, `${label}\n  actual:   ${a}\n  expected: ${b}`);
}

/// Mirrors the plugin host's `RawJsonNumber`: the original token stays in
/// `raw`, and `toJSON` refuses to guess. Redefined here rather than imported
/// because the plugin cannot reach host internals and the selftest has to run
/// without the host on the path.
class HostRawNumber {
  constructor(raw) {
    this.raw = raw;
    Object.freeze(this);
  }
  valueOf() {
    return Number(this.raw);
  }
  toJSON() {
    throw new Error("raw JSON number cannot be emitted without an explicit schema conversion");
  }
}

// ── Part 1: translation units ───────────────────────────────────────

function testRequestTranslation() {
  const manifest = JSON.parse(
    readFileSync(join(dirname(fileURLToPath(import.meta.url)), "rebon-plugin.json"), "utf8"),
  );
  assert(
    manifest.capabilities?.modelProviders?.["deepseek"]?.capabilities?.anchoredMinimal ===
      true,
    "manifest opts deepseek into Anchored Minimal",
  );
  const declared = manifest.capabilities?.modelProviders?.["deepseek"];
  assertEq(
    Object.keys(declared?.models ?? {}),
    ["deepseek-v4-pro", "deepseek-flash", "deepseek-v4-flash"],
    "manifest exposes the renamed Flash id and keeps the legacy one",
  );
  assertEq(declared?.profiles?.small, "deepseek-flash", "the small profile follows the rename");

  const request = {
    model: "deepseek-flash",
    system: "sys",
    transientContext: "volatile",
    maxTokens: 4096,
    temperature: 0.5,
    stopSequences: ["END"],
    reasoningEffort: "xhigh",
    toolChoice: { type: "any" },
    webSearch: { maxUses: 3 },
    tools: [{ name: "Bash", description: "run", inputSchema: { type: "object" } }],
    messages: [
      { role: "user", content: [{ type: "text", text: "hi" }] },
      {
        role: "assistant",
        content: [
          { type: "thinking", thinking: "secret", signature: null, data: null },
          { type: "text", text: "ok" },
          { type: "tool_use", id: "call_1", name: "Bash", input: { command: "ls" } },
        ],
      },
      {
        role: "user",
        content: [
          {
            type: "tool_result",
            tool_use_id: "call_1",
            content: [
              { type: "text", text: "file.txt" },
              { type: "image", source: { type: "base64", mediaType: "image/png", data: "x" } },
            ],
            is_error: false,
          },
        ],
      },
    ],
  };
  const connection = {
    requestOptions: {
      omitBodyFields: ["temperature"],
      body: { top_p: 0.9 },
      extraBody: { ds_flag: true },
    },
    modelRequestOptions: {
      "deepseek-flash": { body: { top_p: 0.8 } },
    },
  };
  const body = buildRequestBody(request, connection);

  assertEq(body.model, "deepseek-flash", "model passes through");
  assertEq(body.instructions, "sys", "transient context does not invalidate stable instructions");
  assertEq(body.max_output_tokens, 4096, "maxTokens → max_output_tokens");
  assert(!("temperature" in body), "omitBodyFields removes temperature");
  assertEq(body.top_p, 0.8, "per-model options override provider options");
  assertEq(body.ds_flag, true, "extraBody merged");
  assert(!("stop" in body) && !("stop_sequences" in body), "stopSequences dropped");
  assertEq(body.store, false, "store:false always");
  assertEq(body.stream, true, "stream:true always");
  assertEq(body.tool_choice, "required", "toolChoice any → required");
  assertEq(body.reasoning, { effort: "max" }, "xhigh is sent as the highest accepted effort (max)");
  assertEq(body.tools.length, 2, "function tool + web_search tool");
  assertEq(body.tools[1], { type: "web_search" }, "webSearch config enables tool");

  const input = body.input;
  assertEq(input[0], { role: "user", content: [{ type: "input_text", text: "hi" }] }, "user text");
  assertEq(
    input[1],
    {
      type: "reasoning",
      id: "rs_call_1",
      status: "completed",
      summary: [],
      content: [{ type: "reasoning_text", text: "secret" }],
    },
    "tool-call turn replays reasoning ahead of its content (required by the API)",
  );
  assertEq(
    input[2],
    { role: "assistant", content: [{ type: "output_text", text: "ok" }] },
    "assistant text",
  );
  assertEq(
    input[3],
    { type: "function_call", call_id: "call_1", name: "Bash", arguments: '{"command":"ls"}' },
    "tool_use → function_call",
  );
  assertEq(
    input[4],
    { type: "function_call_output", call_id: "call_1", output: "file.txt" },
    "tool_result text stays the call's output on its own",
  );
  assertEq(
    input[5],
    {
      role: "user",
      content: [
        { type: "input_text", text: "file.txt" },
        { type: "input_image", image_url: "data:image/png;base64,x" },
      ],
    },
    "a tool result's image follows its call as an input_image part",
  );
  assertEq(
    input[6],
    {
      role: "user",
      content: [
        {
          type: "input_text",
          text: "<system-reminder>\n<runtime_context>\nvolatile\n</runtime_context>\n</system-reminder>",
        },
      ],
    },
    "transient context follows a tool result as a trailing user message",
  );

  // A turn with no tool call must NOT replay reasoning: the server ignores it
  // there, and sending it back would cost tokens on every replay.
  assertEq(
    buildInput([
      {
        role: "assistant",
        content: [
          { type: "thinking", thinking: "plain turn reasoning" },
          { type: "text", text: "done" },
        ],
      },
    ]),
    [{ role: "assistant", content: [{ type: "output_text", text: "done" }] }],
    "plain assistant turn still drops reasoning",
  );

  assertEq(
    withTransientContext([], "context-only"),
    [
      {
        role: "user",
        content: [
          {
            type: "text",
            text: "<system-reminder>\n<runtime_context>\ncontext-only\n</runtime_context>\n</system-reminder>",
          },
        ],
      },
    ],
    "transient context becomes a canonical user message when history is empty",
  );
  assertEq(
    withTransientContext(
      [{ role: "user", content: [{ type: "text", text: "hello" }] }],
      "context",
    ),
    [
      {
        role: "user",
        content: [
          { type: "text", text: "hello" },
          {
            type: "text",
            text: "<system-reminder>\n<runtime_context>\ncontext\n</runtime_context>\n</system-reminder>",
          },
        ],
      },
    ],
    "transient context appends to a current user turn without tool results",
  );
  assertEq(withTransientContext([], ""), [], "empty transient context leaves messages unchanged");

  const image = (data, mediaType = "image/png") => ({
    type: "image",
    source: { type: "base64", mediaType, data },
  });
  assertEq(
    buildInput([
      { role: "user", content: [image("AAAA"), { type: "text", text: "what is this" }] },
    ]),
    [
      {
        role: "user",
        content: [
          { type: "input_image", image_url: "data:image/png;base64,AAAA" },
          { type: "input_text", text: "what is this" },
        ],
      },
    ],
    "user image → input_image part carrying a data URL",
  );

  assertEq(
    buildInput([
      {
        role: "user",
        content: [
          { type: "text", text: "before" },
          image("BBBB", "image/jpeg"),
          { type: "text", text: "after" },
        ],
      },
    ]),
    [
      {
        role: "user",
        content: [
          { type: "input_text", text: "before" },
          { type: "input_image", image_url: "data:image/jpeg;base64,BBBB" },
          { type: "input_text", text: "after" },
        ],
      },
    ],
    "text around an image keeps its order and its media type",
  );

  assertEq(
    buildInput([
      {
        role: "user",
        content: [
          {
            type: "tool_result",
            tool_use_id: "call_img",
            content: [image("CCCC"), image("DDDD", "image/webp")],
            is_error: false,
          },
        ],
      },
    ]),
    [
      { type: "function_call_output", call_id: "call_img", output: "" },
      {
        role: "user",
        content: [
          { type: "input_image", image_url: "data:image/png;base64,CCCC" },
          { type: "input_image", image_url: "data:image/webp;base64,DDDD" },
        ],
      },
    ],
    "an image-only tool result carries both images and no text parts",
  );

  // Two images read in parallel, results in completion order. A user message
  // between the two outputs made DeepSeek 400 with "No tool output found".
  assertEq(
    buildInput([
      {
        role: "user",
        content: [
          { type: "tool_result", tool_use_id: "call_01_b", content: [image("BBBB")], is_error: false },
          { type: "tool_result", tool_use_id: "call_00_a", content: [image("AAAA")], is_error: false },
          { type: "text", text: "go on" },
        ],
      },
    ]),
    [
      { type: "function_call_output", call_id: "call_01_b", output: "" },
      { type: "function_call_output", call_id: "call_00_a", output: "" },
      {
        role: "user",
        content: [
          { type: "input_image", image_url: "data:image/png;base64,BBBB" },
          { type: "input_image", image_url: "data:image/png;base64,AAAA" },
        ],
      },
      { role: "user", content: [{ type: "input_text", text: "go on" }] },
    ],
    "parallel image results send every output before any image message",
  );

  assertEq(
    buildInput([
      {
        role: "user",
        content: [
          {
            type: "tool_result",
            tool_use_id: "call_doc",
            content: [
              { type: "text", text: "report" },
              { type: "document", source: { type: "base64", mediaType: "application/pdf", data: "ZZ" } },
            ],
            is_error: false,
          },
        ],
      },
    ]),
    [
      {
        type: "function_call_output",
        call_id: "call_doc",
        output: "report\n[document omitted: not supported by deepseek-responses]",
      },
    ],
    "documents stay placeholder text in the call's output, with no extra message",
  );

  assertEq(
    buildInput([
      {
        role: "assistant",
        content: [image("EEEE"), { type: "text", text: "see above" }],
      },
    ]),
    [
      {
        role: "assistant",
        content: [
          {
            type: "output_text",
            text: "[image omitted: images are only accepted in user messages]\nsee above",
          },
        ],
      },
    ],
    "an image outside a user message keeps placeholder text (the endpoint 400s on it)",
  );

  const wsInput = buildInput([
    {
      role: "assistant",
      content: [
        { type: "server_tool_use", id: "ws_1", name: "web_search", input: { query: "rust" } },
        {
          type: "web_search_result",
          tool_use_id: "ws_1",
          results: [{ title: "t", url: "u", snippet: null }],
          raw_content: null,
        },
        { type: "text", text: "answer" },
      ],
    },
  ]);
  assertEq(
    wsInput[0],
    { type: "web_search_call", id: "ws_1", status: "completed", action: { query: "rust" } },
    "server_tool_use replays as web_search_call item (server restores results)",
  );
  assertEq(
    wsInput[1],
    { role: "assistant", content: [{ type: "output_text", text: "answer" }] },
    "web_search_result dropped from replay, text kept",
  );

  assertEq(translateReasoningEffort("max"), "max", "max passes through");
  assertEq(translateReasoningEffort("low"), "low", "low passes");
  assertEq(translateReasoningEffort("high"), "high", "high passes");
  assertEq(translateReasoningEffort("none"), "none", "none passes");
  assertEq(translateReasoningEffort("medium"), "medium", "medium passes (the server maps it)");
  assertEq(translateReasoningEffort("minimal"), "minimal", "minimal passes (the server maps it)");
  assertEq(translateReasoningEffort("xhigh"), "max", "xhigh → max (the server would run it as high)");
  assertEq(translateReasoningEffort(undefined), undefined, "absent effort omitted");

  const disabledThinking = buildRequestBody(
    {
      model: "deepseek-v4-flash",
      messages: [{ role: "user", content: [{ type: "text", text: "sum" }] }],
      maxTokens: 64,
      stream: true,
      thinking: { type: "disabled" },
      reasoningEffort: "low",
    },
    null,
  );
  assertEq(
    disabledThinking.reasoning,
    { effort: "none" },
    "thinking disabled → effort none (compact summaries skip reasoning)",
  );

  assertEq(errorTypeForStatus(401), "authentication_error", "401 error type");
  assertEq(errorTypeForStatus(429), "rate_limit_error", "429 error type");
  assertEq(errorTypeForStatus(503), "overloaded_error", "503 error type");
  assertEq(errorTypeForStatus(500), "api_error", "500 error type");

  assertEq(
    translateUsage({
      input_tokens: 10,
      output_tokens: 20,
      input_tokens_details: { cached_tokens: 4 },
      output_tokens_details: { reasoning_tokens: 7 },
    }),
    {
      inputTokens: 10,
      outputTokens: 20,
      promptCacheHitTokens: 4,
      promptCacheMissTokens: 6,
      reasoningTokens: 7,
    },
    "cached_tokens maps to hit/miss (input includes cache; no double-billing)",
  );
  assertEq(
    translateUsage({ input_tokens: 10, prompt_cache_hit_tokens: 6, prompt_cache_miss_tokens: 4 }),
    { inputTokens: 10, promptCacheHitTokens: 6, promptCacheMissTokens: 4 },
    "explicit hit/miss fields pass through without cacheRead double-count",
  );

  const merged = applyRequestOptions({ a: 1, b: 2 }, { omitBodyFields: ["a"], extraBody: { c: 3 } });
  assertEq(merged, { b: 2, c: 3 }, "applyRequestOptions omit + extraBody");

  const cacheRequest = {
    model: "deepseek-v4-flash",
    messages: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
    maxTokens: 64,
    stream: true,
    extensions: { promptCache: { key: "rebon-session-abc", retention: "session" } },
  };
  const cacheDefault = buildRequestBody(cacheRequest, null);
  assertEq(
    cacheDefault.prompt_cache_key,
    "rebon-session-abc",
    "host session cache key forwards by default",
  );
  assert(
    !("prompt_cache_retention" in cacheDefault),
    "internal retention hint never reaches the wire on its own",
  );
  const cacheTuned = buildRequestBody(cacheRequest, {
    requestOptions: {
      omitBodyFields: ["prompt_cache_key"],
      extraBody: { prompt_cache_retention: "24h" },
    },
  });
  assert(
    !("prompt_cache_key" in cacheTuned),
    "omitBodyFields opts back out of cache key forwarding",
  );
  assertEq(
    cacheTuned.prompt_cache_retention,
    "24h",
    "extraBody pins the gateway cache retention",
  );
  assert(
    !("promptCache" in cacheDefault) && !("extensions" in cacheDefault),
    "cache plumbing never leaks extension keys into the body",
  );
  const noCacheContext = buildRequestBody(
    { model: "deepseek-v4-flash", messages: [], maxTokens: 64, stream: true },
    null,
  );
  assert(
    !("prompt_cache_key" in noCacheContext),
    "requests without a promptCache extension stay unchanged",
  );

  const seededConnection = {
    requestOptions: {
      extraBody: {
        rebonAnchoredPreset: "seeded",
        ds_flag: true,
        instructions: "must still be removed",
      },
    },
  };
  const seeded = buildRequestBody(
    {
      model: "deepseek-v4-flash",
      system: "host persona",
      transientContext: "promoted runtime context",
      messages: [{ role: "user", content: [{ type: "text", text: "review the code" }] }],
      tools: [{ name: "ToolSearch", description: "discover", inputSchema: { type: "object" } }],
      maxTokens: 1024,
      stream: true,
      extensions: { promptCache: { key: "rebon-session-seeded", retention: "session" } },
    },
    seededConnection,
  );
  assert(!("instructions" in seeded), "seeded preset clears system instructions after option merging");
  assertEq(
    seeded.input.slice(0, 3),
    [
      { role: "user", content: [{ type: "input_text", text: "how are you" }] },
      {
        type: "reasoning",
        id: "rs_rebon_seeded_standard",
        status: "completed",
        summary: [],
        content: [
          { type: "reasoning_text", text: "We need answer user greeting. Need just respond." },
        ],
      },
      {
        role: "assistant",
        content: [
          { type: "output_text", text: "I’m doing well, thank you! How can I help you today?" },
        ],
      },
    ],
    "seeded preset prepends the fixed user/reasoning/assistant turn",
  );
  assertEq(
    seeded.input[3],
    {
      role: "user",
      content: [
        {
          type: "input_text",
          text: "review the code\n<system-reminder>\n<runtime_context>\npromoted runtime context\n</runtime_context>\n</system-reminder>",
        },
      ],
    },
    "seeded preset preserves the host runtime context after the fixed prefix",
  );
  assertEq(seeded.tools[0].name, "ToolSearch", "seeded preset preserves the host tool projection");
  assertEq(seeded.ds_flag, true, "seeded preset preserves ordinary extraBody fields");
  assert(!("rebonAnchoredPreset" in seeded), "seeded preset control never leaks to DeepSeek");

  const seededControlOnlyConnection = {
    requestOptions: { extraBody: { rebonAnchoredPreset: "seeded" } },
  };
  const compactWithSeedConfigured = buildRequestBody(
    {
      model: "deepseek-v4-flash",
      system: "summarizer",
      messages: [{ role: "user", content: [{ type: "text", text: "summarize" }] }],
      maxTokens: 64,
      stream: true,
      thinking: { type: "disabled" },
      extensions: { promptCache: { key: "rebon-session-seeded", retention: "session" } },
    },
    seededControlOnlyConnection,
  );
  assertEq(compactWithSeedConfigured.instructions, "summarizer", "compaction keeps its own system prompt");
  assertEq(
    compactWithSeedConfigured.input,
    [{ role: "user", content: [{ type: "input_text", text: "summarize" }] }],
    "compaction does not receive the seeded conversation prefix",
  );

  const helperRequestWithSeedConfigured = buildRequestBody(
    {
      model: "deepseek-v4-flash",
      system: "helper",
      messages: [{ role: "user", content: [{ type: "text", text: "helper task" }] }],
      maxTokens: 64,
      stream: true,
    },
    seededControlOnlyConnection,
  );
  assertEq(helperRequestWithSeedConfigured.instructions, "helper", "non-session helper requests stay unchanged");
  assertEq(helperRequestWithSeedConfigured.input.length, 1, "non-session helper requests do not receive the seed");

  let invalidPresetError = "";
  try {
    buildRequestBody(cacheRequest, {
      requestOptions: { extraBody: { rebonAnchoredPreset: "unknown" } },
    });
  } catch (err) {
    invalidPresetError = String(err?.message ?? err);
  }
  assert(
    invalidPresetError.includes('must be "off" or "seeded"'),
    "invalid seeded preset values fail request translation",
  );
}

function testRawJsonNumbers() {
  const plain = {
    text: 'quote " backslash \\ newline \n unicode é',
    integer: 42,
    fraction: 0.5,
    exponent: 1e21,
    negativeZero: -0,
    flag: true,
    nothing: null,
    list: [1, "two", null, { three: 3 }, undefined],
    skipped: undefined,
  };
  assertEq(
    stringifyJson(plain),
    JSON.stringify(plain),
    "plain values serialize exactly as JSON.stringify",
  );

  assertEq(stringifyJson(new HostRawNumber("0.95")), "0.95", "wrapped fraction keeps its token");
  assertEq(stringifyJson(new HostRawNumber("1e-7")), "1e-7", "wrapped exponent keeps its token");
  assertEq(
    stringifyJson(new HostRawNumber("12345678901234567890")),
    "12345678901234567890",
    "wrapped unsafe integer keeps every digit",
  );
  assertEq(
    stringifyJson({ a: [new HostRawNumber("-0.5"), new HostRawNumber("2E+3")] }),
    '{"a":[-0.5,2E+3]}',
    "wrapped numbers survive nesting",
  );

  assertEq(
    stringifyJson({ raw: "0.95", nested: { raw: "1e3" } }),
    '{"raw":"0.95","nested":{"raw":"1e3"}}',
    "ordinary raw fields stay data",
  );

  const big = "12345678901234567890";
  const request = {
    model: "deepseek-v4-flash",
    temperature: new HostRawNumber("0.95"),
    maxTokens: 64,
    stream: true,
    tools: [
      {
        name: "mcp",
        description: "tool",
        inputSchema: {
          type: "object",
          properties: { ratio: { type: "number", minimum: new HostRawNumber("0.5") } },
        },
      },
    ],
    messages: [
      { role: "user", content: [{ type: "text", text: "go" }] },
      {
        role: "assistant",
        content: [
          {
            type: "tool_use",
            id: "call_raw",
            name: "mcp",
            input: { minimum: new HostRawNumber("0.5"), big: new HostRawNumber(big), raw: "kept" },
          },
        ],
      },
      { role: "user", content: [{ type: "tool_result", tool_use_id: "call_raw", content: "done" }] },
    ],
  };
  const connection = {
    requestOptions: {
      extraBody: { top_p: new HostRawNumber("0.95"), payload: { raw: "0.95" } },
    },
  };
  const body = buildRequestBody(request, connection);
  const text = stringifyJson(body);
  assert(text.includes('"temperature":0.95'), "request body carries the wrapped temperature token");
  assert(text.includes('"top_p":0.95'), "extraBody keeps its wrapped number token");
  assert(text.includes('"minimum":0.5'), "a tool schema minimum keeps its fractional token");
  assert(
    text.includes('"payload":{"raw":"0.95"}'),
    "extraBody's ordinary raw field stays an object",
  );

  const functionCall = body.input.find((item) => item.type === "function_call");
  assertEq(
    functionCall.arguments,
    `{"minimum":0.5,"big":${big},"raw":"kept"}`,
    "replayed tool-call arguments keep the wrapped token and the ordinary raw field",
  );
}

async function testDumpSerialization() {
  const dir = mkdtempSync(join(tmpdir(), "rebon-deepseek-dump-"));
  const path = join(dir, "requests.jsonl");
  process.env.REBON_DEEPSEEK_DUMP = path;
  try {
    buildRequestBody(
      {
        model: "deepseek-v4-flash",
        messages: [],
        maxTokens: 8,
        stream: true,
        temperature: new HostRawNumber("0.95"),
      },
      null,
    );
    const deadline = Date.now() + 2000;
    let dump = "";
    while (!dump && Date.now() < deadline) {
      try {
        dump = readFileSync(path, "utf8");
      } catch {
        dump = "";
      }
      if (!dump) await new Promise((resolve) => setTimeout(resolve, 20));
    }
    assert(dump.includes('"temperature":0.95'), "request dump keeps the wrapped number token");
    assert(!dump.includes('"raw":"0.95"'), "request dump does not leak the wrapper");
  } finally {
    delete process.env.REBON_DEEPSEEK_DUMP;
    rmSync(dir, { recursive: true, force: true });
  }
}

function testStreamTranslation() {
  const t = new ResponseEventTranslator("s1");
  const out = [];
  const feed = (event) => out.push(...t.translate(event));

  feed({ type: "response.created", response: { id: "resp_1", model: "deepseek-v4-flash" } });
  feed({ type: "response.output_item.added", item: { id: "rs_1", type: "reasoning" } });
  feed({ type: "response.reasoning_text.delta", item_id: "rs_1", delta: "let me think" });
  feed({ type: "response.output_item.done", item: { id: "rs_1", type: "reasoning" } });
  feed({ type: "response.output_item.added", item: { id: "msg_1", type: "message" } });
  feed({ type: "response.output_text.delta", item_id: "msg_1", delta: "Hello " });
  feed({ type: "response.output_text.delta", item_id: "msg_1", delta: "world" });
  feed({ type: "response.output_item.done", item: { id: "msg_1", type: "message" } });
  feed({
    type: "response.output_item.added",
    item: { id: "fc_1", type: "function_call", call_id: "call_9", name: "Bash" },
  });
  feed({ type: "response.function_call_arguments.delta", item_id: "fc_1", delta: '{"command":' });
  feed({ type: "response.function_call_arguments.delta", item_id: "fc_1", delta: '"ls"}' });
  feed({ type: "response.output_item.done", item: { id: "fc_1", type: "function_call" } });
  feed({
    type: "response.completed",
    response: {
      usage: {
        input_tokens: 10,
        output_tokens: 20,
        input_tokens_details: { cached_tokens: 4 },
        output_tokens_details: { reasoning_tokens: 7 },
      },
    },
  });

  assertEq(
    out[0],
    { type: "message_start", message_id: "resp_1", model: "deepseek-v4-flash", usage: {} },
    "message_start",
  );
  assertEq(
    out[1],
    { type: "content_block_start", index: 0, content_block: { type: "thinking", thinking: "" } },
    "thinking block opens at index 0",
  );
  assertEq(
    out[2],
    {
      type: "content_block_delta",
      index: 0,
      delta: { type: "thinking_delta", thinking: "let me think" },
    },
    "reasoning_text.delta → thinking_delta",
  );
  assertEq(out[3], { type: "content_block_stop", index: 0 }, "thinking closes");
  assertEq(
    out[4],
    { type: "content_block_start", index: 1, content_block: { type: "text", text: "" } },
    "text block opens at index 1",
  );
  assertEq(
    out[7],
    { type: "content_block_stop", index: 1 },
    "text closes",
  );
  assertEq(
    out[8],
    {
      type: "content_block_start",
      index: 2,
      content_block: { type: "tool_use", id: "call_9", name: "Bash" },
    },
    "function_call → tool_use with call_id",
  );
  assertEq(
    out[out.length - 2],
    {
      type: "message_delta",
      delta: {
        stopReason: "tool_use",
        usage: {
          inputTokens: 10,
          outputTokens: 20,
          promptCacheHitTokens: 4,
          promptCacheMissTokens: 6,
          reasoningTokens: 7,
        },
      },
    },
    "message_delta carries tool_use stop + usage",
  );
  assertEq(out[out.length - 1], { type: "message_stop" }, "message_stop terminal");

  // Custom tool call: non-JSON input gets wrapped, valid JSON passes raw.
  const t2 = new ResponseEventTranslator("s2");
  const out2 = [];
  out2.push(...t2.translate({ type: "response.created", response: { id: "r2" } }));
  out2.push(
    ...t2.translate({
      type: "response.output_item.added",
      item: { id: "ct_1", type: "custom_tool_call", call_id: "cc_1", name: "grep" },
    }),
  );
  out2.push(
    ...t2.translate({ type: "response.custom_tool_call_input.delta", item_id: "ct_1", delta: "-rn foo" }),
  );
  out2.push(...t2.translate({ type: "response.output_item.done", item: { id: "ct_1", type: "custom_tool_call" } }));
  const customDelta = out2.find((e) => e.type === "content_block_delta");
  assertEq(
    customDelta.delta,
    { type: "input_json_delta", partial_json: '{"input":"-rn foo"}' },
    "custom tool input wrapped into JSON object",
  );

  // Incomplete → max_tokens; failed → error.
  const t3 = new ResponseEventTranslator("s3");
  t3.translate({ type: "response.created", response: { id: "r3" } });
  const inc = t3.translate({
    type: "response.incomplete",
    response: { incomplete_details: { reason: "max_output_tokens" }, usage: {} },
  });
  assertEq(inc[0].delta.stopReason, "max_tokens", "incomplete → max_tokens");

  const tContentFilter = new ResponseEventTranslator("s3b");
  tContentFilter.translate({ type: "response.created", response: { id: "r3b" } });
  const filtered = tContentFilter.translate({
    type: "response.incomplete",
    response: { incomplete_details: { reason: "content_filter" }, usage: {} },
  });
  assertEq(filtered[0].delta.stopReason, "refusal", "content_filter → refusal");

  const t4 = new ResponseEventTranslator("s4");
  const failed = t4.translate({
    type: "response.failed",
    response: { error: { code: "rate_limit_exceeded", message: "slow down" } },
  });
  assertEq(
    failed[0],
    { type: "error", error_type: "rate_limit_exceeded", message: "slow down" },
    "response.failed → error event",
  );

  // Web search call materializes as a closed server_tool_use block.
  const t5 = new ResponseEventTranslator("s5");
  t5.translate({ type: "response.created", response: { id: "r5" } });
  const ws = t5.translate({
    type: "response.output_item.done",
    item: { id: "ws_1", type: "web_search_call", action: { query: "rust" } },
  });
  assertEq(
    ws[0].content_block,
    { type: "server_tool_use", id: "ws_1", name: "web_search", input: { query: "rust" } },
    "web_search_call → server_tool_use",
  );
  assertEq(ws[1].type, "content_block_stop", "server_tool_use closes immediately");
}

function testResumedToolResults() {
  const interrupted = { type: "text", text: "[Request interrupted by user]" };
  const continued = { role: "user", content: [{ type: "text", text: "请继续" }] };
  const result = {
    type: "tool_result",
    tool_use_id: "call_interrupted",
    content: "[Tool result missing — the `Bash` call was interrupted]",
    is_error: true,
  };
  const secondResult = {
    type: "tool_result",
    tool_use_id: "call_completed",
    content: [{ type: "text", text: "完成" }],
    is_error: false,
  };
  const cases = [
    ["中止文本在前", [interrupted, result]],
    ["工具结果在前", [result, interrupted]],
    ["多工具结果穿插文本", [result, interrupted, secondResult]],
    ["多个文本夹着结果", [interrupted, result, { type: "text", text: "继续" }]],
    ["纯工具结果", [result]],
  ];
  for (const [label, content] of cases) {
    for (const thinking of [[], [{ type: "thinking", thinking: "检查构建结果" }]]) {
      const results = content.filter((block) => block.type === "tool_result");
      const messages = [
        { role: "user", content: [{ type: "text", text: "历史摘要" }] },
        {
          role: "assistant",
          content: [
            ...thinking,
            ...results.map((block) => ({
              type: "tool_use", id: block.tool_use_id, name: "Bash", input: {},
            })),
          ],
        },
        { role: "user", content },
        continued,
        continued,
      ];
      const original = JSON.stringify(messages);
      const request = { model: "deepseek-v4-flash", messages, reasoningEffort: "high" };
      const body = buildRequestBody(request, {});
      const input = body.input;
      const lastCall = input.findLastIndex((item) => item.type === "function_call");
      assertEq(
        input.slice(lastCall + 1, lastCall + 1 + results.length),
        results.map((block) => ({
          type: "function_call_output",
          call_id: block.tool_use_id,
          output: typeof block.content === "string" ? block.content : "完成",
        })),
        `${label}：所有结果在下一条 user 文本之前闭合工具调用`,
      );
      const texts = content.filter((block) => block.type === "text");
      assertEq(
        input.slice(lastCall + 1 + results.length),
        [
          ...(texts.length ? [{
            role: "user",
            content: [{ type: "input_text", text: texts.map((block) => block.text).join("\n") }],
          }] : []),
          ...[continued, continued].map(() => ({
            role: "user", content: [{ type: "input_text", text: "请继续" }],
          })),
        ],
        `${label}：保留中止文本和重复继续请求`,
      );
      assertEq(
        input.filter((item) => item.type === "reasoning").map((item) => item.content[0].text),
        thinking.map((block) => block.thinking),
        `${label}：不丢失或伪造历史 reasoning`,
      );
      assertEq(JSON.stringify(messages), original, `${label}：不修改原始历史`);
      assertEq(buildRequestBody(request, {}), body, `${label}：重放结果稳定`);
    }
  }
  assertEq(
    buildInput([{
      role: "assistant",
      content: [
        { type: "text", text: "先检查" },
        { type: "tool_use", id: "call_order", name: "Read", input: {} },
        { type: "text", text: "再继续" },
      ],
    }]).map((item) => item.type ?? item.role),
    ["assistant", "function_call", "assistant"],
    "assistant 文本与工具调用的顺序保持不变",
  );
}

function testReasoningRequestOptions() {
  const request = (extra = {}) => ({
    model: "deepseek-v4-pro",
    messages: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
    stream: true,
    ...extra,
  });

  const configured = buildRequestBody(request(), {
    requestOptions: {
      body: { reasoning_effort: "xhigh" },
      extraBody: { thinking: { type: "enabled" }, top_p: 0.9 },
    },
  });
  assertEq(
    configured.reasoning,
    { effort: "max" },
    "configured thinkingEffort xhigh → reasoning.effort max",
  );
  assert(!("reasoning_effort" in configured), "chat-style reasoning_effort is not sent upstream");
  assert(!("thinking" in configured), "chat-style thinking switch is not sent upstream");
  assertEq(configured.top_p, 0.9, "unrelated request-option fields still merge");

  const disabled = buildRequestBody(request(), {
    requestOptions: {
      body: { reasoning_effort: "max" },
      extraBody: { thinking: { type: "disabled" } },
    },
  });
  assertEq(disabled.reasoning, { effort: "none" }, "options thinking disabled → effort none");

  const perModelDisabled = buildRequestBody(request(), {
    modelRequestOptions: { "deepseek-v4-pro": { extraBody: { thinking: { type: "disabled" } } } },
  });
  assertEq(
    perModelDisabled.reasoning,
    { effort: "none" },
    "per-model thinking disabled → effort none",
  );

  const extraBodyEffort = buildRequestBody(request(), {
    requestOptions: { extraBody: { reasoning_effort: "max" } },
  });
  assertEq(extraBodyEffort.reasoning, { effort: "max" }, "extraBody reasoning_effort is consumed too");

  const perModelWins = buildRequestBody(request(), {
    requestOptions: { body: { reasoning_effort: "low" } },
    modelRequestOptions: { "deepseek-v4-pro": { body: { reasoning_effort: "xhigh" } } },
  });
  assertEq(perModelWins.reasoning, { effort: "max" }, "per-model options outrank connection options");

  const turnWins = buildRequestBody(request({ reasoningEffort: "low" }), {
    requestOptions: { body: { reasoning_effort: "max" } },
    modelRequestOptions: { "deepseek-v4-pro": { body: { reasoning_effort: "max" } } },
  });
  assertEq(turnWins.reasoning, { effort: "low" }, "the turn's effort outranks request options");

  const turnDisabled = buildRequestBody(
    request({ reasoningEffort: "max", thinking: { type: "disabled" } }),
    { requestOptions: { body: { reasoning_effort: "low" } } },
  );
  assertEq(
    turnDisabled.reasoning,
    { effort: "none" },
    "thinking disabled outranks the turn's effort",
  );

  const explicitWins = buildRequestBody(request({ reasoningEffort: "low" }), {
    requestOptions: {
      body: { reasoning_effort: "low" },
      extraBody: { thinking: { type: "disabled" } },
    },
    modelRequestOptions: {
      "deepseek-v4-pro": { body: { reasoning: { effort: "xhigh", summary: "auto" } } },
    },
  });
  assertEq(
    explicitWins.reasoning,
    { effort: "max", summary: "auto" },
    "explicit reasoning wins over the turn and is translated too",
  );
  assert(
    !("reasoning_effort" in explicitWins) && !("thinking" in explicitWins),
    "explicit reasoning leaves no chat-style fields",
  );

  const explicitFromConnection = buildRequestBody(request({ reasoningEffort: "low" }), {
    requestOptions: {
      body: { reasoning: { effort: "low" } },
      extraBody: { reasoning: { effort: "xhigh" } },
    },
  });
  assertEq(
    explicitFromConnection.reasoning,
    { effort: "max" },
    "extraBody reasoning wins over body reasoning and outranks the turn",
  );

  const plain = buildRequestBody(request(), { requestOptions: { body: { top_p: 0.5 } } });
  assert(!("reasoning" in plain), "no effort anywhere → no reasoning field (server default)");

  const retention = buildRequestBody(request(), {
    requestOptions: { extraBody: { prompt_cache_retention: "24h" } },
  });
  assertEq(retention.prompt_cache_retention, "24h", "extraBody retention still merges");

  const omitted = buildRequestBody(request({ reasoningEffort: "high" }), {
    requestOptions: { omitBodyFields: ["reasoning"] },
  });
  assert(!("reasoning" in omitted), "omitBodyFields still removes the generated reasoning");
}

// ── Part 2: end-to-end against a mock SSE server ────────────────────

const SSE_SCRIPT = [
  { type: "response.created", response: { id: "resp_e2e", model: "deepseek-v4-flash" } },
  { type: "response.output_item.added", item: { id: "rs", type: "reasoning" } },
  { type: "response.reasoning_text.delta", item_id: "rs", delta: "hmm" },
  { type: "response.output_item.done", item: { id: "rs", type: "reasoning" } },
  { type: "response.output_item.added", item: { id: "m", type: "message" } },
  { type: "response.output_text.delta", item_id: "m", delta: "e2e ok" },
  { type: "response.output_item.done", item: { id: "m", type: "message" } },
  {
    type: "response.completed",
    response: { usage: { input_tokens: 3, output_tokens: 5, output_tokens_details: { reasoning_tokens: 2 } } },
  },
];

function startMockServer(seen) {
  return new Promise((resolve) => {
    const server = createServer((req, res) => {
      let body = "";
      req.on("data", (chunk) => (body += chunk));
      req.on("end", () => {
        seen.push({ url: req.url, auth: req.headers.authorization, body: JSON.parse(body), text: body });
        if (req.url !== "/responses") {
          res.writeHead(404).end();
          return;
        }
        const pendingCalls = new Set();
        for (const item of seen[seen.length - 1].body.input) {
          if (item.type === "function_call") pendingCalls.add(item.call_id);
          if (item.type === "function_call_output") pendingCalls.delete(item.call_id);
          if (item.role === "user" && pendingCalls.size > 0) break;
        }
        if (pendingCalls.size > 0) {
          res.writeHead(400, { "Content-Type": "application/json" });
          res.end(JSON.stringify({ error: {
            message: `No tool output found for tool call ${pendingCalls.values().next().value}.`,
          } }));
          return;
        }
        if (seen[seen.length - 1].body.model === "hang-forever") {
          res.writeHead(200, { "Content-Type": "text/event-stream" });
          res.write(`data: ${JSON.stringify(SSE_SCRIPT[0])}\n\n`);
          return; // never completes; exercises cancel
        }
        res.writeHead(200, { "Content-Type": "text/event-stream" });
        for (const event of SSE_SCRIPT) {
          res.write(`event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`);
        }
        res.end();
      });
    });
    server.listen(0, "127.0.0.1", () => resolve(server));
  });
}

async function testEndToEnd() {
  const seen = [];
  const server = await startMockServer(seen);
  const port = server.address().port;

  // The plugin contract, faked in ten lines rather than imported: `activate`
  // is handed a registrar, registers one adapter, and is then called as
  // `handler(turn, ctx)` with an `emit` and an abort signal. Faking it keeps
  // this package's selftest runnable without rebon's host on the path, and
  // still exercises exactly the surface the host drives.
  const registered = {};
  const api = {
    llm(provider, handler, info) {
      registered.provider = provider;
      registered.handler = handler;
      registered.info = info;
    },
  };
  activate(api);
  assertEq(registered.provider, "deepseek", "e2e: registers under its provider id");
  assert(
    registered.info?.capabilities?.forcedToolChoice === true,
    "e2e: the ready report advertises forcedToolChoice",
  );
  assert(
    !("reasoningText" in (registered.info?.capabilities ?? {})) &&
      !("anchoredMinimal" in (registered.info?.capabilities ?? {})),
    "e2e: the report leaves the manifest-declared capabilities to the manifest",
  );
  assertEq(registered.info?.protocolVersion, 1, "e2e: the report names the protocol version");

  const connection = {
    baseUrl: `http://127.0.0.1:${port}`,
    apiKey: "sk-e2e",
    headers: { "X-E2E": "1" },
  };
  const turn = (model, signal) => [
    {
      protocolVersion: 1,
      connection,
      request: {
        model,
        messages: [{ role: "user", content: [{ type: "text", text: "hello" }] }],
        maxTokens: 128,
        stream: true,
      },
    },
    { signal, emit: undefined },
  ];

  // A full turn.
  const events = [];
  {
    const [payload, ctx] = turn("deepseek-v4-flash", new AbortController().signal);
    ctx.emit = async (event) => events.push(event);
    await registered.handler(payload, ctx);
  }
  assertEq(events[0].type, "message_start", "e2e: first event is message_start");
  assert(
    events.some((e) => e.type === "content_block_delta" && e.delta.type === "thinking_delta"),
    "e2e: thinking delta streamed",
  );
  assert(
    events.some((e) => e.type === "content_block_delta" && e.delta.type === "text_delta" && e.delta.text === "e2e ok"),
    "e2e: text delta streamed",
  );
  const messageDelta = events.find((e) => e.type === "message_delta");
  assertEq(
    messageDelta.delta.usage,
    { inputTokens: 3, outputTokens: 5, reasoningTokens: 2 },
    "e2e: usage carries reasoningTokens",
  );
  assertEq(events[events.length - 1].type, "message_stop", "e2e: terminal message_stop");

  const request = seen[0];
  assertEq(request.auth, "Bearer sk-e2e", "e2e: connection apiKey used as bearer");
  assertEq(request.body.store, false, "e2e: store:false sent");

  {
    const [payload, ctx] = turn("deepseek-v4-flash", new AbortController().signal);
    payload.request.messages = [
      { role: "user", content: [{ type: "text", text: "历史摘要" }] },
      { role: "assistant", content: [{
        type: "tool_use", id: "call_resume", name: "Bash", input: {},
      }] },
      { role: "user", content: [
        { type: "text", text: "[Request interrupted by user]" },
        {
          type: "tool_result", tool_use_id: "call_resume",
          content: "[Tool result missing — the `Bash` call was interrupted]", is_error: true,
        },
      ] },
      { role: "user", content: [{ type: "text", text: "请继续" }] },
    ];
    const resumed = [];
    ctx.emit = async (event) => resumed.push(event);
    await registered.handler(payload, ctx);
    assert(!resumed.some((event) => event.type === "error"), "e2e：中止后重放不触发配对 400");
    assertEq(resumed.at(-1)?.type, "message_stop", "e2e：中止后重放正常完成");
    assertEq(
      seen.at(-1).body.input.map((item) => item.type ?? item.role),
      ["user", "function_call", "function_call_output", "user", "user"],
      "e2e：实际发送的结果先于中止文本",
    );
  }

  // Host raw numbers: wrappers in the request and in extraBody reach the wire
  // as the exact tokens the host received.
  {
    const [payload, ctx] = turn("deepseek-v4-flash", new AbortController().signal);
    payload.connection = {
      ...connection,
      requestOptions: { extraBody: { top_p: new HostRawNumber("0.95") } },
    };
    payload.request.temperature = new HostRawNumber("0.7");
    payload.request.messages = [
      { role: "user", content: [{ type: "text", text: "start" }] },
      {
        role: "assistant",
        content: [
          {
            type: "tool_use",
            id: "call_raw",
            name: "mcp",
            input: { minimum: new HostRawNumber("0.5"), big: new HostRawNumber("12345678901234567890"), raw: "kept" },
          },
        ],
      },
      { role: "user", content: [{ type: "tool_result", tool_use_id: "call_raw", content: "done" }] },
      { role: "user", content: [{ type: "text", text: "continue" }] },
    ];
    const rawEvents = [];
    ctx.emit = async (event) => rawEvents.push(event);
    await registered.handler(payload, ctx);
    assert(!rawEvents.some((event) => event.type === "error"), "e2e: raw numbers never fail the turn");
    const sent = seen.at(-1);
    assertEq(sent.body.temperature, 0.7, "e2e: wrapped temperature crosses the wire");
    assertEq(sent.body.top_p, 0.95, "e2e: wrapped extraBody value crosses the wire");
    assert(sent.text.includes('"temperature":0.7'), "e2e: temperature keeps its exact token");
    assert(sent.text.includes('"top_p":0.95'), "e2e: extraBody keeps its exact token");
    const call = sent.body.input.find((item) => item.type === "function_call");
    assertEq(
      call.arguments,
      '{"minimum":0.5,"big":12345678901234567890,"raw":"kept"}',
      "e2e: replayed tool-call arguments keep the exact number text",
    );
  }

  // Cancel: a turn the host stops reading aborts, and emits no terminal.
  {
    const controller = new AbortController();
    const [payload, ctx] = turn("hang-forever", controller.signal);
    const hung = [];
    ctx.emit = async (event) => hung.push(event);
    const running = registered.handler(payload, ctx);
    await new Promise((resolve) => setTimeout(resolve, 200));
    controller.abort();
    await running;
    assert(
      !hung.some((e) => ["message_stop", "error"].includes(e.type)),
      "e2e: a cancelled turn emits no terminal event",
    );
  }

  server.close();
}


// ── Runner ──────────────────────────────────────────────────────────

testRequestTranslation();
testReasoningRequestOptions();
testRawJsonNumbers();
testResumedToolResults();
testStreamTranslation();
await testDumpSerialization();
await testEndToEnd();

if (failures > 0) {
  console.error(`selftest: ${failures}/${checks} checks FAILED`);
  process.exit(1);
}
console.log(`selftest: ${checks} checks passed`);
