// Payload schemas for the methods that carry one. Mirrors
// `crates/rebon-plugin-protocol/src/methods.rs`; the shared corpus at
// `crates/rebon-plugin-protocol/tests/fixtures/v1/methods.json` is what keeps
// the two honest. Codes are the bracketed tokens that corpus pins, which is a
// different vocabulary from `protocol.mjs`'s framing codes on purpose: they
// answer different questions and are pinned by different corpora.
//
// Validation order is part of the contract. A payload with two problems reports
// the first one in declaration order, and both languages must pick the same one.
import { ProtocolError, PLATFORM_PLUGIN_ID, plain } from './protocol.mjs';

export const PLUGIN_LOAD_METHOD = 'plugin/load';
export const PLUGIN_UNLOAD_METHOD = 'plugin/unload';
export const SERVICE_CALL_METHOD = 'service/call';
export const EVENT_DELIVER_METHOD = 'event/deliver';
export const EVENT_SUBSCRIBE_METHOD = 'event/subscribe';
export const EVENT_UNSUBSCRIBE_METHOD = 'event/unsubscribe';
export const EVENT_EMIT_METHOD = 'event/emit';
export const TOOL_INVOKE_METHOD = 'tool/invoke';
export const LLM_STREAM_METHOD = 'llm/stream';
export const LLM_CONTROL_METHOD = 'llm/control';
export const COMMAND_INVOKE_METHOD = 'command/invoke';
// The surfaces a command can say it works on, and the two groupings a menu
// knows. Closed sets, mirroring `rebon-slash-commands`.
export const COMMAND_SURFACES = Object.freeze(['tui', 'desktop', 'acp', 'web', 'mobile']);
export const COMMAND_CATEGORIES = Object.freeze(['command', 'agent']);
// The conversation-level signals an adapter can be told about. A closed set:
// an open string would make every unknown signal look like a typo.
export const LLM_CONTROL_SIGNALS = Object.freeze(['reset', 'endTurn', 'invalidate']);
export const TOOL_CALL_METHOD = 'tool/call';
export const SEAT_CALL_METHOD = 'seat/call';

export const MAX_NAME_BYTES = 128;
export const MAX_DECLARATIONS = 256;
// A tool description is read by a model, so it is spent out of every turn's
// context budget for as long as the tool is offered. The bound is part of the
// contract, not a defensive habit.
export const MAX_DESCRIPTION_BYTES = 4096;

// Rust bounds a name by its UTF-8 byte length. Measuring UTF-16 units here
// would accept names Rust rejects, which is exactly the kind of drift a plugin
// author discovers only on the other host.
const byteLength = (value) => Buffer.byteLength(value, 'utf8');
// `\p{Cc}` is the Unicode category Rust's `char::is_control` tests.
const CONTROL = /\p{Cc}/u;

function shape(value, required, optional, where) {
  if (!plain(value)) throw new ProtocolError('[WRONG_SHAPE]', `${where} must be a plain object`);
  const known = new Set([...required, ...optional]);
  for (const key of Object.keys(value)) {
    if (!known.has(key)) throw new ProtocolError('[UNKNOWN_FIELD]', `${where} has unknown field ${JSON.stringify(key)}`);
  }
  for (const key of required) {
    if (!Object.prototype.hasOwnProperty.call(value, key)) {
      throw new ProtocolError('[MISSING_FIELD]', `${where} is missing field ${JSON.stringify(key)}`);
    }
  }
}

function string(value, where) {
  if (typeof value !== 'string') throw new ProtocolError('[WRONG_SHAPE]', `${where} must be a string`);
  return value;
}

export function validateName(kind, name) {
  if (name.length === 0) throw new ProtocolError('[EMPTY_NAME]', `${kind} name must not be empty`);
  if (byteLength(name) > MAX_NAME_BYTES) throw new ProtocolError('[NAME_TOO_LONG]', `${kind} name is longer than ${MAX_NAME_BYTES} bytes`);
  if (CONTROL.test(name)) throw new ProtocolError('[CONTROL_CHARACTER]', `${kind} name contains a control character`);
  return name;
}

export const MAX_PATH_BYTES = 4096;

// `path.isAbsolute` answers for the platform it runs on, which is right for
// using a path and wrong for agreeing about one: this rule is read by both
// languages on whichever machine runs CI. A supervisor and its host always run
// on the same machine, so both spellings count.
const looksAbsolute = (value) => /^[/\\]/.test(value) || /^[A-Za-z]:[/\\]/.test(value);

function pathShape(kind, value) {
  if (value.length === 0) throw new ProtocolError('[EMPTY_NAME]', `${kind} path must not be empty`);
  if (byteLength(value) > MAX_PATH_BYTES) throw new ProtocolError('[PATH_TOO_LONG]', `${kind} path is longer than ${MAX_PATH_BYTES} bytes`);
  if (CONTROL.test(value)) throw new ProtocolError('[CONTROL_CHARACTER]', `${kind} path contains a control character`);
  return value;
}

export function validateAbsolutePath(kind, value) {
  pathShape(kind, value);
  if (!looksAbsolute(value)) throw new ProtocolError('[PATH_NOT_ABSOLUTE]', `${kind} path must be absolute`);
  return value;
}

export function validateRelativePath(kind, value) {
  pathShape(kind, value);
  if (looksAbsolute(value)) throw new ProtocolError('[PATH_NOT_RELATIVE]', `${kind} path must be relative to the package root`);
  if (value.includes('\\') || value.split('/').some((part) => part === '..' || part === '')) {
    throw new ProtocolError('[PATH_ESCAPES]', `${kind} path leaves the package root`);
  }
  return value;
}

export function validatePluginId(pluginId) {
  validateName('plugin', pluginId);
  if (pluginId === PLATFORM_PLUGIN_ID) throw new ProtocolError('[RESERVED_PLUGIN_ID]', `plugin id ${PLATFORM_PLUGIN_ID} is reserved for platform control traffic`);
  return pluginId;
}

// Absent lists default to empty, matching serde's `#[serde(default)]`. The
// length bound is checked before the entries, and each entry's own validity
// before the duplicate check — same order as Rust.
function declarations(value, kind, where) {
  if (value === undefined) return [];
  if (!Array.isArray(value)) throw new ProtocolError('[WRONG_SHAPE]', `${where} must be an array`);
  if (value.length > MAX_DECLARATIONS) throw new ProtocolError('[TOO_MANY_DECLARATIONS]', `${where} declares ${value.length} entries, over the ${MAX_DECLARATIONS} limit`);
  const seen = new Set();
  for (const entry of value) {
    validateName(kind, string(entry, `${where} entry`));
    if (seen.has(entry)) throw new ProtocolError('[DUPLICATE_DECLARATION]', `${where} declares ${JSON.stringify(entry)} twice`);
    seen.add(entry);
  }
  return Object.freeze([...value]);
}

// A command as a plugin describes it: one-to-one with `CommandSpec`, minus
// the two kinds a plugin may not claim (`native` has no front-end function to
// map an id to, `session` is rebon's own engine state).
export function pluginCommandDefinition(input) {
  shape(input, ['name', 'description', 'kind'], ['aliases', 'zhAliases', 'hint', 'category', 'surfaces'], 'command definition');
  const name = validateName('command', string(input.name, 'name'));
  for (const alias of input.aliases ?? []) validateName('command', string(alias, 'alias'));
  for (const alias of input.zhAliases ?? []) validateName('command', string(alias, 'zh alias'));
  const description = string(input.description, 'description');
  if (description.length === 0) throw new ProtocolError('[EMPTY_DESCRIPTION]', `command ${JSON.stringify(name)} has no description`);
  if (byteLength(description) > MAX_DESCRIPTION_BYTES) throw new ProtocolError('[DESCRIPTION_TOO_LONG]', `command ${JSON.stringify(name)} has a description over the ${MAX_DESCRIPTION_BYTES} byte limit`);
  if (input.category !== undefined && !COMMAND_CATEGORIES.includes(input.category)) {
    throw new ProtocolError('[WRONG_SHAPE]', `command category must be one of ${COMMAND_CATEGORIES.join(', ')}`);
  }
  for (const surface of input.surfaces ?? []) {
    if (!COMMAND_SURFACES.includes(surface)) {
      throw new ProtocolError('[WRONG_SHAPE]', `command surface must be one of ${COMMAND_SURFACES.join(', ')}`);
    }
  }
  const kind = input.kind;
  if (!plain(kind)) throw new ProtocolError('[WRONG_SHAPE]', 'command kind must be a plain object');
  switch (kind.type) {
    case 'prompt':
      shape(kind, ['type'], [], 'command kind');
      break;
    case 'explain': {
      shape(kind, ['type', 'text'], [], 'command kind');
      const text = string(kind.text, 'kind text');
      if (text.length === 0) throw new ProtocolError('[EMPTY_DESCRIPTION]', `command ${JSON.stringify(name)} explains nothing`);
      break;
    }
    case 'panel':
      shape(kind, ['type', 'dialog'], [], 'command kind');
      validateName('dialog', string(kind.dialog, 'kind dialog'));
      break;
    default:
      throw new ProtocolError('[WRONG_SHAPE]', 'command kind must be prompt, explain or panel');
  }
  return Object.freeze({ ...input });
}

function commandDefinitions(value, where) {
  if (value === undefined) return Object.freeze([]);
  if (!Array.isArray(value)) throw new ProtocolError('[WRONG_SHAPE]', `${where} must be an array`);
  if (value.length > MAX_DECLARATIONS) throw new ProtocolError('[TOO_MANY_DECLARATIONS]', `${where} declares ${value.length} entries, over the ${MAX_DECLARATIONS} limit`);
  const seen = new Set();
  const out = [];
  for (const entry of value) {
    const command = pluginCommandDefinition(entry);
    if (seen.has(command.name)) throw new ProtocolError('[DUPLICATE_DECLARATION]', `${where} declares ${JSON.stringify(command.name)} twice`);
    seen.add(command.name);
    out.push(command);
  }
  return Object.freeze(out);
}

export function pluginLoadRequest(input) {
  shape(input, ['pluginId', 'root', 'entry'], ['services', 'eventTopics', 'publishedTopics', 'llmProviders', 'tools', 'commands', 'invokableTools', 'seats', 'config'], 'plugin/load payload');
  return Object.freeze({
    pluginId: validatePluginId(string(input.pluginId, 'pluginId')),
    root: validateAbsolutePath('root', string(input.root, 'root')),
    entry: validateRelativePath('entry', string(input.entry, 'entry')),
    services: declarations(input.services, 'service', 'services'),
    eventTopics: declarations(input.eventTopics, 'topic', 'eventTopics'),
    // What it may publish, as opposed to what it listens to. Two powers, two
    // lists — the same separation `tools` and `invokableTools` keep.
    publishedTopics: declarations(input.publishedTopics, 'topic', 'publishedTopics'),
    llmProviders: declarations(input.llmProviders, 'provider', 'llmProviders'),
    tools: declarations(input.tools, 'tool', 'tools'),
    // Command names only. What a command looks like in a menu comes with the
    // registration; the name is the reviewable part, and the part that can
    // collide with a built-in.
    commands: declarations(input.commands, 'command', 'commands'),
    // The one declared list with no registered counterpart: a plugin calls
    // these rather than providing them, which is why its name says so.
    invokableTools: declarations(input.invokableTools, 'tool', 'invokableTools'),
    seats: declarations(input.seats, 'seat', 'seats'),
    // Opaque: what a plugin's configuration means is the plugin's contract, and
    // an absent one is `null` rather than a missing field — the same rule every
    // other payload slot follows.
    config: input.config === undefined ? null : input.config,
  });
}

export function pluginToolDefinition(input) {
  shape(input, ['name', 'description', 'inputSchema'], [], 'tool definition');
  const name = validateName('tool', string(input.name, 'name'));
  const description = string(input.description, 'description');
  if (description.length === 0) throw new ProtocolError('[EMPTY_DESCRIPTION]', `tool ${JSON.stringify(name)} has no description, so a model cannot know when to use it`);
  if (byteLength(description) > MAX_DESCRIPTION_BYTES) throw new ProtocolError('[DESCRIPTION_TOO_LONG]', `tool ${JSON.stringify(name)} has a description over the ${MAX_DESCRIPTION_BYTES} byte limit`);
  return Object.freeze({ name, description, inputSchema: input.inputSchema });
}

function toolDefinitions(value, where) {
  if (value === undefined) return [];
  if (!Array.isArray(value)) throw new ProtocolError('[WRONG_SHAPE]', `${where} must be an array`);
  if (value.length > MAX_DECLARATIONS) throw new ProtocolError('[TOO_MANY_DECLARATIONS]', `${where} declares ${value.length} entries, over the ${MAX_DECLARATIONS} limit`);
  const seen = new Set();
  const out = [];
  for (const entry of value) {
    const tool = pluginToolDefinition(entry);
    if (seen.has(tool.name)) throw new ProtocolError('[DUPLICATE_DECLARATION]', `${where} declares ${JSON.stringify(tool.name)} twice`);
    seen.add(tool.name);
    out.push(tool);
  }
  return Object.freeze(out);
}

// What each adapter says about itself, keyed by the provider it serves. The
// values stay opaque — the wire layer decides only whether a route may be
// served, and `llmProviders` answers that — so the only rule here is that a
// description must belong to a route the same report claims.
function llmAdapters(value, providers, where) {
  if (value === undefined) return Object.freeze({});
  if (!plain(value)) throw new ProtocolError('[WRONG_SHAPE]', `${where} must be a plain object`);
  const declared = new Set(providers);
  for (const provider of Object.keys(value)) {
    validateName('provider', provider);
    if (!declared.has(provider)) {
      throw new ProtocolError('[UNDECLARED_ADAPTER]', `adapter info describes provider ${JSON.stringify(provider)}, which this report does not serve`);
    }
  }
  return Object.freeze({ ...value });
}

export function pluginReadyReport(input) {
  shape(input, ['pluginId'], ['services', 'eventTopics', 'llmProviders', 'llmAdapters', 'tools', 'commands'], 'plugin/load terminal payload');
  // Field order, not convenience order: a payload with two problems has to
  // report the same one here and in Rust, and `llmAdapters` is checked against
  // `llmProviders` so the providers have to be read first — but not before the
  // fields Rust reads before them.
  const pluginId = validatePluginId(string(input.pluginId, 'pluginId'));
  const services = declarations(input.services, 'service', 'services');
  const eventTopics = declarations(input.eventTopics, 'topic', 'eventTopics');
  const providers = declarations(input.llmProviders, 'provider', 'llmProviders');
  return Object.freeze({
    pluginId,
    services,
    eventTopics,
    llmProviders: providers,
    llmAdapters: llmAdapters(input.llmAdapters, providers, 'llmAdapters'),
    tools: toolDefinitions(input.tools, 'tools'),
    commands: commandDefinitions(input.commands, 'commands'),
  });
}

export function pluginUnloadRequest(input) {
  shape(input, ['pluginId'], [], 'plugin/unload payload');
  return Object.freeze({ pluginId: validatePluginId(string(input.pluginId, 'pluginId')) });
}

export function pluginDrainReport(input) {
  shape(input, ['pluginId'], ['outstandingCalls', 'revokedSubscriptions'], 'plugin/unload terminal payload');
  return Object.freeze({
    pluginId: validatePluginId(string(input.pluginId, 'pluginId')),
    outstandingCalls: declarations(input.outstandingCalls, 'call', 'outstandingCalls'),
    revokedSubscriptions: declarations(input.revokedSubscriptions, 'subscription', 'revokedSubscriptions'),
  });
}

export function serviceCallRequest(input) {
  shape(input, ['service', 'request'], [], 'service/call payload');
  return Object.freeze({ service: validateName('service', string(input.service, 'service')), request: input.request });
}

export function eventSubscribeRequest(input) {
  shape(input, ['subscription', 'topic'], [], 'event/subscribe payload');
  return Object.freeze({
    subscription: validateName('subscription', string(input.subscription, 'subscription')),
    topic: validateName('topic', string(input.topic, 'topic')),
  });
}

export function eventUnsubscribeRequest(input) {
  shape(input, ['subscription'], [], 'event/unsubscribe payload');
  return Object.freeze({ subscription: validateName('subscription', string(input.subscription, 'subscription')) });
}

export function eventDelivery(input) {
  shape(input, ['subscription', 'topic', 'event'], [], 'event/deliver payload');
  return Object.freeze({
    subscription: validateName('subscription', string(input.subscription, 'subscription')),
    topic: validateName('topic', string(input.topic, 'topic')),
    event: input.event,
  });
}

export function eventEmitRequest(input) {
  shape(input, ['topic', 'event'], [], 'event/emit payload');
  return Object.freeze({
    topic: validateName('topic', string(input.topic, 'topic')),
    event: input.event,
  });
}

export function toolInvokeRequest(input) {
  shape(input, ['tool', 'input'], [], 'tool/invoke payload');
  return Object.freeze({ tool: validateName('tool', string(input.tool, 'tool')), input: input.input });
}

export function llmStreamRequest(input) {
  shape(input, ['provider', 'request'], [], 'llm/stream payload');
  return Object.freeze({ provider: validateName('provider', string(input.provider, 'provider')), request: input.request });
}

export function llmControlRequest(input) {
  shape(input, ['provider', 'signal'], [], 'llm/control payload');
  // Provider first, matching the Rust validator's field order.
  const provider = validateName('provider', string(input.provider, 'provider'));
  const signal = string(input.signal, 'signal');
  if (!LLM_CONTROL_SIGNALS.includes(signal)) {
    throw new ProtocolError('[UNKNOWN_LLM_SIGNAL]', `llm control signal ${JSON.stringify(signal)} is not one of ${LLM_CONTROL_SIGNALS.join(', ')}`);
  }
  return Object.freeze({ provider, signal });
}

export function commandInvokeRequest(input) {
  shape(input, ['name', 'raw', 'rest', 'surface'], [], 'command/invoke payload');
  const name = validateName('command', string(input.name, 'name'));
  const raw = string(input.raw, 'raw');
  const rest = string(input.rest, 'rest');
  const surface = string(input.surface, 'surface');
  if (!COMMAND_SURFACES.includes(surface)) {
    throw new ProtocolError('[WRONG_SHAPE]', `surface must be one of ${COMMAND_SURFACES.join(', ')}`);
  }
  return Object.freeze({ name, raw, rest, surface });
}

export function seatCallRequest(input) {
  shape(input, ['seat', 'method', 'params'], [], 'seat/call payload');
  return Object.freeze({
    seat: validateName('seat', string(input.seat, 'seat')),
    method: validateName('method', string(input.method, 'method')),
    params: input.params,
  });
}

/// The corpus's `kind` names, so a shared case table can dispatch by name.
export const VALIDATORS = Object.freeze({
  plugin_load: pluginLoadRequest,
  plugin_ready: pluginReadyReport,
  plugin_unload: pluginUnloadRequest,
  plugin_drain: pluginDrainReport,
  service_call: serviceCallRequest,
  event_subscribe: eventSubscribeRequest,
  event_unsubscribe: eventUnsubscribeRequest,
  event_deliver: eventDelivery,
  event_emit: eventEmitRequest,
  tool_invoke: toolInvokeRequest,
  llm_stream: llmStreamRequest,
  llm_control: llmControlRequest,
  command_invoke: commandInvokeRequest,
  seat_call: seatCallRequest,
});
