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
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  activate,
  applyRequestOptions,
  buildInput,
  buildRequestBody,
  errorTypeForStatus,
  ResponseEventTranslator,
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

  const request = {
    model: "deepseek-v4-flash",
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
      "deepseek-v4-flash": { body: { top_p: 0.8 } },
    },
  };
  const body = buildRequestBody(request, connection);

  assertEq(body.model, "deepseek-v4-flash", "model passes through");
  assertEq(body.instructions, "sys", "transient context does not invalidate stable instructions");
  assertEq(body.max_output_tokens, 4096, "maxTokens → max_output_tokens");
  assert(!("temperature" in body), "omitBodyFields removes temperature");
  assertEq(body.top_p, 0.8, "per-model options override provider options");
  assertEq(body.ds_flag, true, "extraBody merged");
  assert(!("stop" in body) && !("stop_sequences" in body), "stopSequences dropped");
  assertEq(body.store, false, "store:false always");
  assertEq(body.stream, true, "stream:true always");
  assertEq(body.tool_choice, "required", "toolChoice any → required");
  assertEq(body.reasoning, { effort: "xhigh" }, "xhigh passes through natively");
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
    {
      type: "function_call_output",
      call_id: "call_1",
      output: "file.txt\n[image omitted: not supported by deepseek-responses]",
    },
    "tool_result → function_call_output with image placeholder",
  );
  assertEq(
    input[5],
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

  const imgInput = buildInput([
    {
      role: "user",
      content: [
        { type: "image", source: {} },
        { type: "text", text: "what is this" },
      ],
    },
  ]);
  assertEq(
    imgInput,
    [
      {
        role: "user",
        content: [
          {
            type: "input_text",
            text: "[image omitted: this model does not accept image input]\nwhat is this",
          },
        ],
      },
    ],
    "image input replaced with placeholder text (matches upstream behavior)",
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

  assertEq(translateReasoningEffort("max"), "max", "max passes through natively");
  assertEq(translateReasoningEffort("low"), "low", "low passes");
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
        seen.push({ url: req.url, auth: req.headers.authorization, body: JSON.parse(body) });
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
testResumedToolResults();
testStreamTranslation();
await testEndToEnd();

if (failures > 0) {
  console.error(`selftest: ${failures}/${checks} checks FAILED`);
  process.exit(1);
}
console.log(`selftest: ${checks} checks passed`);
