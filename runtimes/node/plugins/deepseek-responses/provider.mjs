#!/usr/bin/env node
// DeepSeek Responses model provider plugin for Rebon.
//
// A plugin on rebon's plugin plane: `activate` registers one `llm/stream`
// adapter under the provider id, is handed one turn at a time as a
// `ModelProviderTurnV1` (`{protocolVersion, connection, request}`), and answers
// with `StreamEventV1` chunks. The payload vocabulary is the same
// `rebon.modelProvider` v1 one this provider has always spoken; what changed
// is that it no longer carries its own transport.
//
// Upstream is the DeepSeek Responses API (`POST {base}/responses`, SSE).
// This file owns the whole DeepSeek dialect so Rebon core never grows
// `if is_deepseek` branches:
//   - request translation and parameter trimming (no service_tier, no
//     previous_response_id, store:false, no stop_sequences; the host's
//     prompt_cache_key is forwarded only when the connection opts in — see
//     buildRequestBody),
//   - image blocks -> `input_image` parts carrying a base64 data URL; the
//     endpoint takes images in user messages only, so tool-result images
//     follow their call in a user message of their own,
//   - `response.reasoning_text.delta` -> thinking_delta,
//   - `response.custom_tool_call_input.delta` -> tool_use input,
//   - `response.web_search_call.*` -> server_tool_use blocks,
//   - error-code and usage translation (reasoning/cache token details).
//
// Diagnostics go to stderr, which the host keeps as a tail and folds into a
// failure message; stdout belongs to the host's own framing.

import { pathToFileURL } from "node:url";

const PROTOCOL_VERSION = 1;
const DEFAULT_BASE_URL = "https://api.deepseek.com";
const ANCHORED_PRESET_OPTION = "rebonAnchoredPreset";
const SEEDED_PRESET = "seeded";
const SEEDED_STANDARD_REASONING = "We need answer user greeting. Need just respond.";
const SEEDED_STANDARD_REPLY = "I’m doing well, thank you! How can I help you today?";

// Only capability fields that every released host parses: the initialize
// The capability set is split across two places on purpose, and the split has
// outlived the reason it was made. It began because an older Rebon decoded the
// `initialize` result with deny_unknown_fields, so a newer capability name
// reported here would have broken it; the newer names went into
// rebon-plugin.json, whose parsing tolerates unknown fields.
//
// The report is still the stricter of the two, so the split stays: this half
// is what the adapter asserts at run time, the manifest half is what the
// package declares, and Rebon takes the union. Nothing is lost by the
// division — a capability set either way reaches the same OR.
const RUNTIME_CAPABILITIES = {
  forcedToolChoice: true,
  webSearch: true,
};

// ── Request translation ─────────────────────────────────────────────

// DeepSeek accepts the full effort range natively (none / minimal / low /
// medium / high / xhigh / max); Rebon's five tiers pass through unchanged.
export function translateReasoningEffort(effort) {
  switch (effort) {
    case "low":
    case "medium":
    case "high":
    case "xhigh":
    case "max":
      return effort;
    default:
      return undefined;
  }
}

/// Splits a tool result into the text lines of its `function_call_output` and
/// the image blocks that follow it.
///
/// A document has no DeepSeek equivalent and keeps the placeholder text it
/// already had; an image is not text, so it leaves the output string entirely
/// and becomes an `input_image` part — see the tool_result arm of buildInput.
function splitToolResult(content) {
  if (typeof content === "string") return { lines: [content], images: [] };
  if (!Array.isArray(content)) return { lines: [], images: [] };
  const lines = [];
  const images = [];
  for (const block of content) {
    switch (block.type) {
      case "text":
        lines.push(block.text);
        break;
      case "document":
        lines.push("[document omitted: not supported by deepseek-responses]");
        break;
      case "image":
        images.push(block);
        break;
      default:
        break;
    }
  }
  return { lines, images };
}

/// One `input_image` part from a Rebon image block.
///
/// The data URL is the whole input: the endpoint reads the format from the
/// bytes rather than from the media type, and `detail` is left off because its
/// default already means "keep the original".
function imageInputPart(block) {
  return {
    type: "input_image",
    image_url: `data:${block.source.mediaType};base64,${block.source.data}`,
  };
}

/// Replayed reasoning for one assistant turn, or "" when it must not be sent.
///
/// DeepSeek rejects a thinking-mode turn that carries a tool call but no
/// replayed reasoning ("The `reasoning_text` in the thinking mode must be
/// passed back to the API") — the whole turn 400s, and the surfaced message is
/// the misleading "No tool output found for tool call <id>". Reasoning is
/// therefore replayed on exactly the turns that carry a tool call, and dropped
/// on plain turns where the server ignores it anyway.
function replayedReasoningText(message) {
  if (message.role !== "assistant") return "";
  if (!message.content.some((block) => block.type === "tool_use")) return "";
  return message.content
    .filter((block) => block.type === "thinking" && block.thinking)
    .map((block) => block.thinking)
    .join("\n");
}

/// Stable id for a replayed reasoning item: derived from the turn's first tool
/// call so a re-sent history keeps a byte-identical prefix for the cache.
function reasoningItemId(message) {
  const call = message.content.find((block) => block.type === "tool_use");
  return `rs_${call?.id ?? "rebon"}`;
}

export function buildInput(messages) {
  const input = [];
  for (const message of messages) {
    const role = message.role;
    const reasoningText = replayedReasoningText(message);
    if (reasoningText) {
      input.push({
        type: "reasoning",
        id: reasoningItemId(message),
        status: "completed",
        summary: [],
        content: [{ type: "reasoning_text", text: reasoningText }],
      });
    }
    // One `{role, content}` item per contiguous run of content, so a turn that
    // interleaves text and a tool call keeps its text where it was.
    let textParts = [];
    let contentParts = [];
    const flushText = () => {
      if (textParts.length === 0) return;
      const type = role === "assistant" ? "output_text" : "input_text";
      contentParts.push({ type, text: textParts.join("\n") });
      textParts = [];
    };
    const flushRun = () => {
      flushText();
      if (contentParts.length === 0) return;
      input.push({ role, content: contentParts });
      contentParts = [];
    };
    // user 文本会开启新回合；必须先交齐上一回合的工具结果，避免 DeepSeek 判定结果缺失。
    const blocks = role === "user"
      ? [
          ...message.content.filter((block) => block.type === "tool_result"),
          ...message.content.filter((block) => block.type !== "tool_result"),
        ]
      : message.content;
    for (const block of blocks) {
      switch (block.type) {
        case "text":
          textParts.push(block.text);
          break;
        case "image":
          // Only user (and developer) messages may carry `input_image`; the
          // endpoint answers an image anywhere else with a 400, so those keep
          // the placeholder text instead.
          if (role !== "user") {
            textParts.push("[image omitted: images are only accepted in user messages]");
            break;
          }
          flushText();
          contentParts.push(imageInputPart(block));
          break;
        case "thinking":
          // Already emitted as a `reasoning` item ahead of this turn's content
          // when the turn calls a tool; otherwise deliberately dropped.
          break;
        case "tool_use":
          flushRun();
          input.push({
            type: "function_call",
            call_id: block.id,
            name: block.name,
            arguments: JSON.stringify(block.input ?? {}),
          });
          break;
        case "tool_result": {
          flushRun();
          const { lines, images } = splitToolResult(block.content);
          input.push({
            type: "function_call_output",
            call_id: block.tool_use_id,
            output: lines.join("\n"),
          });
          if (images.length > 0) {
            // The call's output stays the text string and its images follow in
            // a user message of their own — the shape the built-in Responses
            // path sends, and the only role an image is accepted in.
            input.push({
              role: "user",
              content: [
                ...lines
                  .filter((line) => line !== "")
                  .map((line) => ({ type: "input_text", text: line })),
                ...images.map(imageInputPart),
              ],
            });
          }
          break;
        }
        case "server_tool_use":
          // Replay web_search_call items as-is: the server restores the
          // search results from the call id.
          flushRun();
          input.push({
            type: "web_search_call",
            id: block.id,
            status: "completed",
            action: block.input ?? {},
          });
          break;
        case "web_search_result":
          // Restored server-side from the replayed web_search_call item;
          // a textual copy here would duplicate the results.
          break;
        case "compaction":
          if (block.content) textParts.push(block.content);
          break;
        case "generated_image":
          // Image generation is declared off; ignore stray blocks.
          break;
        default:
          break;
      }
    }
    flushRun();
  }
  return input;
}

function translateToolChoice(choice) {
  if (!choice) return undefined;
  switch (choice.type) {
    case "auto":
      return "auto";
    case "any":
      return "required";
    case "none":
      return "none";
    case "tool":
      return { type: "function", name: choice.name };
    default:
      return undefined;
  }
}

/// `omitBodyFields` removes generated fields, then `body` and `extraBody`
/// merge on top (same order as Rebon's built-in OpenAI options). Keys are
/// DeepSeek wire names, giving users an escape hatch for dialect drift
/// without a plugin release.
export function applyRequestOptions(body, options) {
  if (!options || typeof options !== "object") return body;
  for (const field of options.omitBodyFields ?? []) {
    delete body[field];
  }
  for (const [key, value] of Object.entries(options.body ?? {})) {
    if (key !== ANCHORED_PRESET_OPTION) body[key] = value;
  }
  for (const [key, value] of Object.entries(options.extraBody ?? {})) {
    if (key !== ANCHORED_PRESET_OPTION) body[key] = value;
  }
  return body;
}

function wrapRuntimeContext(context) {
  return `<system-reminder>\n<runtime_context>\n${context}\n</runtime_context>\n</system-reminder>`;
}

export function withTransientContext(messages, transientContext) {
  if (!transientContext) return messages;
  const result = messages.map((message) => ({
    ...message,
    content: [...(message.content ?? [])],
  }));
  const block = { type: "text", text: wrapRuntimeContext(transientContext) };
  const last = result.at(-1);
  if (
    last?.role === "user" &&
    last.content.every((contentBlock) => contentBlock.type !== "tool_result")
  ) {
    last.content.push(block);
  } else {
    result.push({ role: "user", content: [block] });
  }
  return result;
}

function anchoredPresetOption(options) {
  return options?.extraBody?.[ANCHORED_PRESET_OPTION] ?? options?.body?.[ANCHORED_PRESET_OPTION];
}

export function resolveAnchoredPreset(request, connection) {
  const modelOptions = connection?.modelRequestOptions?.[request.model];
  const configured = anchoredPresetOption(modelOptions) ?? anchoredPresetOption(connection?.requestOptions);
  if (configured === undefined || configured === null || configured === "" || configured === "off") {
    return "off";
  }
  if (configured !== SEEDED_PRESET) {
    throw new Error(
      `${ANCHORED_PRESET_OPTION} must be "off" or "${SEEDED_PRESET}", got ${JSON.stringify(configured)}`,
    );
  }
  return configured;
}

export function seededStandardPrefix() {
  return [
    { role: "user", content: [{ type: "input_text", text: "how are you" }] },
    {
      type: "reasoning",
      id: "rs_rebon_seeded_standard",
      status: "completed",
      summary: [],
      content: [{ type: "reasoning_text", text: SEEDED_STANDARD_REASONING }],
    },
    {
      role: "assistant",
      content: [{ type: "output_text", text: SEEDED_STANDARD_REPLY }],
    },
  ];
}

function usesSeededStandard(request, connection) {
  return (
    resolveAnchoredPreset(request, connection) === SEEDED_PRESET &&
    request.thinking?.type !== "disabled" &&
    Boolean(request.extensions?.promptCache?.key)
  );
}

/// Append one outgoing request body to `REBON_DEEPSEEK_DUMP` (a file path).
///
/// Off unless the variable is set. Wire-level bugs in this dialect surface as
/// opaque upstream 400s whose text names the wrong cause, and the body is the
/// only artifact that settles them — reconstructing it from the transcript is
/// not equivalent, as the host may send a different projection than it
/// persisted. Never fails the turn: a dump problem must not break a session.
///
/// `node:fs` is imported here rather than at the top of the file, because a
/// static import is a load-time requirement on every host: the in-process JS
/// runtime offers `node:readline` and `node:url` and nothing else, so one
/// diagnostic would have kept the whole provider from loading there. Imported
/// on demand it stays exactly what it claims to be — off unless asked for, and
/// unable to take a session down when it fails.
function dumpRequestBody(body) {
  const path = process.env.REBON_DEEPSEEK_DUMP;
  if (!path) return;
  const line = `${JSON.stringify({ at: new Date().toISOString(), body })}\n`;
  import("node:fs")
    .then(({ appendFileSync }) => appendFileSync(path, line))
    .catch((err) =>
      console.error(`deepseek-responses: request dump failed: ${err?.message ?? err}`),
    );
}

export function buildRequestBody(request, connection) {
  const seededStandard = usesSeededStandard(request, connection);
  const translatedInput = buildInput(
    withTransientContext(request.messages ?? [], request.transientContext),
  );
  const input = seededStandard
    ? [...seededStandardPrefix(), ...translatedInput]
    : translatedInput;
  const body = {
    model: request.model,
    input,
    stream: true,
    store: false,
  };
  const instructions = request.system ?? "";
  if (instructions && !seededStandard) body.instructions = instructions;
  if (request.maxTokens) body.max_output_tokens = request.maxTokens;
  if (request.temperature !== undefined && request.temperature !== null) {
    body.temperature = request.temperature;
  }
  const tools = (request.tools ?? []).map((tool) => ({
    type: "function",
    name: tool.name,
    description: tool.description,
    parameters: tool.inputSchema,
  }));
  if (request.webSearch) {
    tools.push({ type: "web_search" });
  }
  if (tools.length > 0) body.tools = tools;
  const toolChoice = translateToolChoice(request.toolChoice);
  if (toolChoice !== undefined) body.tool_choice = toolChoice;
  if (request.thinking?.type === "disabled") {
    // effort "none" disables thinking mode — used by compact summarisation
    // requests so the summary does not burn reasoning tokens.
    body.reasoning = { effort: "none" };
  } else {
    const effort = translateReasoningEffort(request.reasoningEffort);
    if (effort) body.reasoning = { effort };
  }
  // Deliberately dropped: stopSequences (unsupported by /responses),
  // metadata, thinking budgets, reasoningMode/Summary.
  //
  // Prompt-cache routing. The host sends a stable per-session key in the
  // promptCache extension; forward it by default — the official DeepSeek
  // endpoint silently ignores it, while OpenAI-compatible gateways with
  // keyed prefix caches (e.g. opencode Go) route and retain by it. Opt out
  // with `omitBodyFields: ["prompt_cache_key"]`, and pin a gateway TTL via
  // `extraBody: { prompt_cache_retention: "24h" }` — both run after this
  // line, so provider options always win.
  const promptCacheKey = request.extensions?.promptCache?.key;
  if (promptCacheKey) {
    body.prompt_cache_key = promptCacheKey;
  }
  applyRequestOptions(body, connection?.requestOptions);
  const modelOptions = connection?.modelRequestOptions?.[request.model];
  applyRequestOptions(body, modelOptions);
  if (seededStandard) delete body.instructions;
  dumpRequestBody(body);
  return body;
}

// ── Usage / error translation ───────────────────────────────────────

export function translateUsage(raw) {
  if (!raw || typeof raw !== "object") return {};
  const usage = {};
  if (raw.input_tokens) usage.inputTokens = raw.input_tokens;
  if (raw.output_tokens) usage.outputTokens = raw.output_tokens;
  // DeepSeek's input_tokens INCLUDES cached tokens (cached_tokens is a
  // subset), so cache accounting must use the hit/miss convention — the
  // Anthropic-style cacheReadInputTokens field assumes an exclusive
  // input_tokens and would double-bill in billed_input_tokens().
  const hit = raw.prompt_cache_hit_tokens ?? raw.input_tokens_details?.cached_tokens ?? 0;
  if (hit) {
    usage.promptCacheHitTokens = hit;
    const miss = raw.prompt_cache_miss_tokens ?? Math.max(0, (raw.input_tokens ?? 0) - hit);
    if (miss) usage.promptCacheMissTokens = miss;
  }
  const reasoning = raw.output_tokens_details?.reasoning_tokens ?? 0;
  if (reasoning) usage.reasoningTokens = reasoning;
  return usage;
}

export function errorTypeForStatus(status) {
  if (status === 401 || status === 403) return "authentication_error";
  if (status === 404) return "not_found_error";
  if (status === 429) return "rate_limit_error";
  if (status === 400 || status === 422) return "invalid_request_error";
  if (status === 503 || status === 529) return "overloaded_error";
  if (status >= 500) return "api_error";
  return "api_error";
}

// ── SSE event translation ───────────────────────────────────────────

/// Translates DeepSeek Responses SSE payloads into StreamEventV1 values.
/// Content-block indices are assigned in arrival order; per-item state is
/// keyed by item id so interleaved deltas route to the right block.
export class ResponseEventTranslator {
  constructor(streamId) {
    this.streamId = streamId;
    this.nextIndex = 0;
    this.items = new Map();
    this.started = false;
    this.sawToolUse = false;
    this.finished = false;
  }

  openItem(itemId, kind, startBlock) {
    const index = this.nextIndex++;
    this.items.set(itemId, { index, kind, buffer: "" });
    return [{ type: "content_block_start", index, content_block: startBlock }];
  }

  ensureItem(itemId, kind, startBlock) {
    const existing = this.items.get(itemId);
    if (existing) return { item: existing, events: [] };
    const events = this.openItem(itemId, kind, startBlock);
    return { item: this.items.get(itemId), events };
  }

  closeItem(itemId) {
    const item = this.items.get(itemId);
    if (!item) return [];
    this.items.delete(itemId);
    const events = [];
    if (item.kind === "custom_tool_call") {
      events.push({
        type: "content_block_delta",
        index: item.index,
        delta: { type: "input_json_delta", partial_json: normalizeCustomToolInput(item.buffer) },
      });
    }
    events.push({ type: "content_block_stop", index: item.index });
    return events;
  }

  closeAll() {
    const events = [];
    for (const itemId of [...this.items.keys()]) {
      events.push(...this.closeItem(itemId));
    }
    return events;
  }

  start(responseId, model, usage) {
    if (this.started) return [];
    this.started = true;
    return [
      {
        type: "message_start",
        message_id: responseId ?? this.streamId,
        model: model ?? "",
        usage: usage ?? {},
      },
    ];
  }

  finish(stopReason, usage) {
    if (this.finished) return [];
    this.finished = true;
    const events = this.closeAll();
    events.push({
      type: "message_delta",
      delta: { stopReason, usage: usage ?? {} },
    });
    events.push({ type: "message_stop" });
    return events;
  }

  fail(errorType, message) {
    if (this.finished) return [];
    this.finished = true;
    return [{ type: "error", error_type: errorType, message }];
  }

  translate(event) {
    const type = event.type ?? "";
    switch (type) {
      case "response.created":
      case "response.in_progress":
        return this.start(event.response?.id, event.response?.model);
      case "response.output_item.added": {
        const item = event.item ?? {};
        const itemId = item.id ?? `item-${this.nextIndex}`;
        switch (item.type) {
          case "message":
            return this.openItem(itemId, "message", { type: "text", text: "" });
          case "reasoning":
            return this.openItem(itemId, "reasoning", { type: "thinking", thinking: "" });
          case "function_call":
            this.sawToolUse = true;
            return this.openItem(itemId, "function_call", {
              type: "tool_use",
              id: item.call_id ?? itemId,
              name: item.name ?? "",
            });
          case "custom_tool_call":
            this.sawToolUse = true;
            return this.openItem(itemId, "custom_tool_call", {
              type: "tool_use",
              id: item.call_id ?? itemId,
              name: item.name ?? "",
            });
          case "web_search_call":
            // Opened lazily at output_item.done so the block can carry the
            // final search action as its input.
            return [];
          default:
            return [];
        }
      }
      case "response.output_text.delta": {
        const { item, events } = this.ensureItem(
          event.item_id ?? "text-fallback",
          "message",
          { type: "text", text: "" },
        );
        events.push({
          type: "content_block_delta",
          index: item.index,
          delta: { type: "text_delta", text: event.delta ?? "" },
        });
        return events;
      }
      case "response.reasoning_text.delta": {
        const { item, events } = this.ensureItem(
          event.item_id ?? "reasoning-fallback",
          "reasoning",
          { type: "thinking", thinking: "" },
        );
        events.push({
          type: "content_block_delta",
          index: item.index,
          delta: { type: "thinking_delta", thinking: event.delta ?? "" },
        });
        return events;
      }
      case "response.function_call_arguments.delta": {
        const item = this.items.get(event.item_id);
        if (!item) return [];
        return [
          {
            type: "content_block_delta",
            index: item.index,
            delta: { type: "input_json_delta", partial_json: event.delta ?? "" },
          },
        ];
      }
      case "response.custom_tool_call_input.delta": {
        const item = this.items.get(event.item_id);
        if (!item) return [];
        // Custom tool input is freeform text; buffer it and emit one valid
        // JSON payload when the item closes, since Rebon parses the
        // concatenated partial_json at content_block_stop.
        item.buffer += event.delta ?? "";
        return [];
      }
      case "response.output_item.done": {
        const item = event.item ?? {};
        if (item.type === "web_search_call") {
          const index = this.nextIndex++;
          return [
            {
              type: "content_block_start",
              index,
              content_block: {
                type: "server_tool_use",
                id: item.id ?? `ws-${index}`,
                name: "web_search",
                input: item.action ?? {},
              },
            },
            { type: "content_block_stop", index },
          ];
        }
        return this.closeItem(item.id ?? "");
      }
      case "response.completed": {
        const usage = translateUsage(event.response?.usage);
        const stopReason = this.sawToolUse ? "tool_use" : "end_turn";
        return this.finish(stopReason, usage);
      }
      case "response.incomplete": {
        const usage = translateUsage(event.response?.usage);
        const reason = event.response?.incomplete_details?.reason ?? "";
        const stopReason = reason.includes("max_output_tokens")
          ? "max_tokens"
          : reason.includes("content_filter")
            ? "refusal"
            : "end_turn";
        return this.finish(stopReason, usage);
      }
      case "response.failed": {
        const error = event.response?.error ?? {};
        return this.fail(error.code ?? "api_error", error.message ?? "response failed");
      }
      case "error":
        return this.fail(event.code ?? "api_error", event.message ?? "provider error");
      default:
        // reasoning summaries, content_part bookkeeping, ping, etc.
        return [];
    }
  }
}

function normalizeCustomToolInput(raw) {
  const trimmed = raw.trim();
  if (trimmed) {
    try {
      const parsed = JSON.parse(trimmed);
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        return trimmed;
      }
    } catch {
      // fall through to wrapping
    }
  }
  return JSON.stringify({ input: raw });
}

// ── SSE parsing ─────────────────────────────────────────────────────

export async function* sseEvents(stream) {
  const decoder = new TextDecoder();
  let buffer = "";
  for await (const chunk of stream) {
    buffer += decoder.decode(chunk, { stream: true });
    let boundary;
    while ((boundary = buffer.indexOf("\n\n")) !== -1) {
      const rawEvent = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      const data = rawEvent
        .split("\n")
        .filter((line) => line.startsWith("data:"))
        .map((line) => line.slice(5).trimStart())
        .join("\n");
      if (!data || data === "[DONE]") continue;
      try {
        yield JSON.parse(data);
      } catch (err) {
        process.stderr.write(`deepseek-responses: malformed SSE data: ${err}\n`);
      }
    }
  }
}

// ── The plugin-plane adapter ────────────────────────────────────────
//
// This provider used to be a child process of its own speaking
// `rebon.modelProvider` over stdio: an `initialize` handshake carrying the
// user's connection settings, a `createMessage` request answered with
// `accepted: true` and then a stream of `modelProvider/streamEvent`
// notifications, plus `cancel` and `shutdown`.
//
// It is a plugin on rebon's shared Node host now, and every one of those
// pieces had a counterpart already waiting there:
//
//   * the handshake → the ready report. `api.llm(id, handler, info)` reports
//     what this provider can do once, before anything routes to it.
//   * `createMessage` → `llm/stream`. One call per turn; `ctx.emit` puts a
//     stream event on it and returning ends it.
//   * `cancel` → `ctx.signal`, which the host raises when rebon stops reading.
//   * `shutdown` → `plugin/unload` draining, which the host owns.
//   * the connection → the turn. It arrives with each request rather than
//     once at startup, because one loaded adapter now serves whichever
//     provider entry the user has selected, and that can change without this
//     module being reloaded.
//
// The DeepSeek half below this line — the request translation, the SSE
// reader, the event translator — is untouched.

/** The provider id this module registers under; the manifest declares the same. */
const PROVIDER_ID = "deepseek";

/** What the ready report tells rebon before it routes a turn here. */
const ADAPTER_INFO = {
  protocolVersion: PROTOCOL_VERSION,
  capabilities: RUNTIME_CAPABILITIES,
  defaultModel: "deepseek-v4-pro",
  displayName: "DeepSeek Responses",
};

class Provider {
  /** @param connection the user's settings for this entry, or null. */
  constructor(connection = null) {
    this.connection = connection ?? null;
  }

  apiKey() {
    const configured = this.connection?.apiKey ?? "";
    return configured || process.env.DEEPSEEK_API_KEY || "";
  }

  responsesUrl() {
    let base = this.connection?.baseUrl || process.env.DEEPSEEK_BASE_URL || DEFAULT_BASE_URL;
    base = base.replace(/\/+$/, "");
    return base.endsWith("/responses") ? base : `${base}/responses`;
  }

  headers() {
    return {
      "Content-Type": "application/json",
      Accept: "text/event-stream",
      Authorization: `Bearer ${this.apiKey()}`,
      ...(this.connection?.headers ?? {}),
    };
  }

  /**
   * One turn. Emits `StreamEventV1`s and returns when the turn is over.
   *
   * Errors are emitted rather than thrown wherever the provider can say
   * something useful about them — an `error` stream event carries a type rebon
   * maps onto its own retry policy, and a throw would flatten that into one
   * opaque failure.
   */
  async stream(request, emit, signal) {
    const translator = new ResponseEventTranslator(PROVIDER_ID);
    const send = async (events) => {
      for (const event of events) await emit(event);
    };
    if (!this.apiKey()) {
      await send(
        translator.fail(
          "authentication_error",
          "DeepSeek API key not configured: set apiKey on the deepseek provider entry or export DEEPSEEK_API_KEY",
        ),
      );
      return;
    }
    let body;
    try {
      body = buildRequestBody(request, this.connection);
    } catch (err) {
      await send(translator.fail("invalid_request_error", `request translation failed: ${err?.message ?? err}`));
      return;
    }

    let response;
    try {
      response = await fetch(this.responsesUrl(), {
        method: "POST",
        headers: this.headers(),
        body: JSON.stringify(body),
        signal,
      });
    } catch (err) {
      if (signal?.aborted) return;
      await send(translator.fail("api_error", `request to DeepSeek failed: ${err?.message ?? err}`));
      return;
    }
    if (!response.ok) {
      const detail = await safeErrorDetail(response);
      await send(
        translator.fail(
          errorTypeForStatus(response.status),
          `DeepSeek returned HTTP ${response.status}${detail ? `: ${detail}` : ""}`,
        ),
      );
      return;
    }
    try {
      for await (const event of sseEvents(response.body)) {
        if (signal?.aborted) return;
        await send(translator.translate(event));
        if (translator.finished) return;
      }
    } catch (err) {
      if (signal?.aborted) return;
      await send(translator.fail("api_error", `DeepSeek stream read failed: ${err?.message ?? err}`));
      return;
    }
    // The stream ended without a terminal `response.*` event.
    await send(translator.fail("api_error", "DeepSeek stream ended without response.completed"));
  }
}

/**
 * The plugin entry point.
 *
 * No `control` handler: this provider keeps nothing between turns, so `reset`,
 * `endTurn` and `invalidate` have nothing to act on, and the host treats a
 * missing handler as the no-op it is.
 */
export function activate(api, config) {
  // Configuration from the load request, if an installation ever supplies
  // one. The connection is deliberately not read from here — it rides the
  // turn, so that changing the API key in settings takes effect on the next
  // turn rather than the next reload.
  const defaults = config ?? null;
  api.llm(
    PROVIDER_ID,
    async (turn, ctx) => {
      const connection = turn?.connection ?? defaults?.connection ?? null;
      const request = turn?.request;
      if (!request) throw new TypeError("llm/stream payload carries no request");
      await new Provider(connection).stream(request, (event) => ctx.emit(event), ctx.signal);
      return null;
    },
    ADAPTER_INFO,
  );
}

async function safeErrorDetail(response) {
  try {
    const text = await response.text();
    try {
      const parsed = JSON.parse(text);
      return parsed.error?.message ?? text.slice(0, 500);
    } catch {
      return text.slice(0, 500);
    }
  } catch {
    return "";
  }
}

/// `node provider.mjs --smoke "prompt"` — one-shot check against the real
/// DeepSeek API using DEEPSEEK_API_KEY / DEEPSEEK_BASE_URL. Streams the
/// answer to stdout and reasoning to stderr; exits non-zero on any error.
async function smoke(prompt) {
  const provider = new Provider();
  if (!provider.apiKey()) {
    console.error("smoke: set DEEPSEEK_API_KEY first");
    process.exit(2);
  }
  const request = {
    // Mirrors the manifest's defaultModel so a smoke check exercises the same
    // model real sessions use; override with DEEPSEEK_MODEL.
    model: process.env.DEEPSEEK_MODEL || "deepseek-v4-pro",
    messages: [{ role: "user", content: [{ type: "text", text: prompt }] }],
    maxTokens: 2048,
    stream: true,
  };
  const body = buildRequestBody(request, provider.connection);
  console.error(`smoke: POST ${provider.responsesUrl()} model=${request.model}`);
  const response = await fetch(provider.responsesUrl(), {
    method: "POST",
    headers: provider.headers(),
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    console.error(`smoke: HTTP ${response.status}: ${await response.text()}`);
    process.exit(1);
  }
  const translator = new ResponseEventTranslator("smoke");
  for await (const event of sseEvents(response.body)) {
    for (const out of translator.translate(event)) {
      if (out.type === "content_block_delta" && out.delta.type === "text_delta") {
        process.stdout.write(out.delta.text);
      } else if (out.type === "content_block_delta" && out.delta.type === "thinking_delta") {
        process.stderr.write(out.delta.thinking);
      } else if (out.type === "content_block_start" && out.content_block.type === "thinking") {
        process.stderr.write("smoke: [reasoning] ");
      } else if (out.type === "content_block_start" && out.content_block.type === "tool_use") {
        process.stderr.write(`\nsmoke: [tool_use ${out.content_block.name}] `);
      } else if (out.type === "message_delta") {
        process.stderr.write(
          `\nsmoke: stop=${out.delta.stopReason} usage=${JSON.stringify(out.delta.usage)}\n`,
        );
      } else if (out.type === "error") {
        console.error(`\nsmoke: ERROR ${out.error_type}: ${out.message}`);
        process.exit(1);
      }
    }
  }
  process.stdout.write("\n");
}

const isMain =
  process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href;
// Run directly, this file is only a smoke check: as a plugin it is imported by
// the host, which calls `activate` rather than executing it.
if (isMain) {
  if (process.argv[2] === "--smoke") {
    smoke(process.argv[3] ?? "Hi, how are you?").catch((err) => {
      console.error(`smoke: ${err?.stack ?? err}`);
      process.exit(1);
    });
  } else {
    console.error("deepseek-responses is a rebon plugin; run it with --smoke to check the API");
    process.exit(2);
  }
}
