// The composition-side glue that lets the REAL dsh agent loop drive a turn
// inside the composition realm.
//
// Ported from `js/compose/loop-assembly.js`. Three duties, all rebon-owned
// (the dsh packages stay unmodified):
//
//  1. **Tool-schema supply** — a dsh systemPrompt tool provider merging the
//     composition's own registered definitions with rebon's core tool catalog,
//     so what the model is offered and what the scheduler can dispatch stay one
//     fact (a local definition shadows a core tool name in both places).
//  2. **Observability** — every dsh `session/event` (chunks included: the
//     embedder's session face streams from them) and `agent/error` is published
//     onto rebon's event plane.
//  3. **Drive** — `loop:control` is the embedder's inbound face
//     (followup / steer / cancel / status).
//
// Two consequences of the transport:
//
//   * **The catalog is configuration, not a call.** There is no `describeTools`
//     method on the plane, and there should not be: what rebon offers a model
//     is settled when the composition is built, so rebon hands it over at
//     load. It is a snapshot either way; asking would only make the moment it
//     was taken harder to see.
//   * **Events publish through a scope handle.** A loop produces turn events on
//     its own schedule, with no inbound call to hang them on, so the assembly
//     takes a handle on each open session and fans its events to whichever are
//     live. A loop exists before any session opens, so the first events are
//     held — boundedly — until one does, and the self-drive kickoff waits for
//     the same moment: an agent that ran a whole turn before anyone was
//     listening would be a turn rebon never saw.
import { createUserMessage, errorChain } from '@deepseek-ai/dsh-llm';
import { logger } from './bridge.mjs';
import { sinkOf } from './registry.mjs';

export const name = 'rebon-loop-assembly';
export const inject = ['systemPrompt', 'tools'];

/** Topics this assembly publishes; a loop plugin declares exactly these. */
export const LOOP_TOPICS = Object.freeze([
  'loop:event',
  'loop:agent-error',
  'loop:agent-created',
]);

export async function apply(ctx, config = {}) {
  const sink = sinkOf(ctx);
  if (sink === undefined) {
    throw new Error('rebon-loop-assembly must be mounted as a composition entry');
  }
  // Embedder-chosen discriminator: with several loops in one composition, every
  // published event carries the owning loop's tag so subscribers can tell them
  // apart.
  const tag = typeof config.tag === 'string' && config.tag.length > 0 ? config.tag : undefined;
  const stamp = (payload) => (tag === undefined ? payload : { tag, ...payload });

  // Sessions this loop is attached to. A published event goes to all of them;
  // in the ordinary case there is exactly one.
  const sessions = new Set();
  // A loop starts existing when it is loaded, and a session opens after — so
  // the first events (an agent being created, above all) happen with nobody
  // listening. Holding them until the first session attaches is the difference
  // between "rebon saw the loop start" and "rebon saw the loop already
  // running"; the bound is here so a loop nobody ever attaches to cannot grow
  // without limit, and passing it is said out loud rather than swallowed.
  const PRE_ATTACH_LIMIT = 256;
  let attached = false;
  let dropped = 0;
  const held = [];
  const deliver = (session, topic, payload) => {
    // Best effort, and never a reason for the loop to fail: an event nobody
    // could be told about is not a turn that went wrong.
    void session.publish(topic, payload).catch((cause) => {
      logger.warn(`loop-assembly: publishing ${topic} failed: ${cause?.message ?? cause}`);
    });
  };
  sink.scope((scopeCtx) => {
    sessions.add(scopeCtx);
    if (!attached) {
      attached = true;
      if (dropped > 0) {
        logger.warn(`loop-assembly: dropped ${dropped} event(s) produced before any session attached`);
      }
      for (const [topic, payload] of held.splice(0)) deliver(scopeCtx, topic, payload);
    }
    kickoff();
    return () => sessions.delete(scopeCtx);
  });
  const announce = (topic, payload) => {
    const stamped = stamp(payload);
    if (!attached) {
      if (held.length >= PRE_ATTACH_LIMIT) {
        held.shift();
        dropped += 1;
      }
      held.push([topic, stamped]);
      return;
    }
    for (const session of sessions) deliver(session, topic, stamped);
  };

  // Optional prompt contribution into the loop realm's REAL dsh systemPrompt
  // (the loop's own prompt plane, not rebon's seat — `isolate` keeps them
  // apart, which is why the composition structure declares it).
  if (config.section !== undefined && config.section !== null && typeof config.section === 'object') {
    ctx.systemPrompt.section(config.section);
  }

  const seatSchemas = (Array.isArray(config.toolCatalog) ? config.toolCatalog : []).map((tool) => ({
    name: String(tool?.name ?? ''),
    description: String(tool?.description ?? ''),
    parameters: tool?.inputSchema ?? tool?.parameters ?? { type: 'object' },
  })).filter((tool) => tool.name.length > 0);

  ctx.systemPrompt.tools(() => {
    const merged = new Map();
    for (const schema of seatSchemas) merged.set(schema.name, schema);
    for (const [toolName, def] of ctx.tools.defs) {
      merged.set(toolName, {
        name: toolName,
        description: String(def.description ?? ''),
        parameters: def.parameters ?? { type: 'object' },
      });
    }
    return { schemas: [...merged.values()] };
  });

  // Full-fidelity relay: the dsh session log IS the loop's truth, and the
  // embedder's session face (streaming UI, transcript projection) needs every
  // event — chunks included.
  ctx.on('session/event', (session, event) => {
    announce('loop:event', {
      sessionId: String(session.id),
      seq: event.seq,
      type: event.type,
      data: event.data ?? null,
    });
  });

  ctx.on('agent/error', (payload) => {
    announce('loop:agent-error', {
      agentId: String(payload.agent?.id ?? ''),
      turn: payload.turn,
      step: payload.step,
      error: errorChain(payload.error),
    });
  });

  // Which agents are this loop's.
  //
  // The dsh `agents` registry is one per composition realm, so with more than
  // one loop mounted every assembly hears every agent being created. Naming the
  // configured ids is how an assembly knows which of them it is responsible
  // for; dsh appends a session suffix to the configured id, so the match is on
  // that stem. With no list the assembly adopts everything, which is the
  // single-loop composition and the honest default for it.
  const owned = Array.isArray(config.agents)
    ? config.agents.map(String).filter((id) => id.length > 0)
    : undefined;
  const isMine = (agentId) => owned === undefined
    || owned.some((id) => agentId === id || agentId.startsWith(`${id}-`));

  const live = new Map();
  const kicked = new Set();
  // The self-drive convenience, deliberately tied to a session rather than to
  // an agent appearing: an agent created before anyone is listening would run a
  // whole turn whose events had nowhere to go.
  function kickoff() {
    const text = config.kickoff;
    if (typeof text !== 'string' || text.length === 0) return;
    if (!attached) return;
    for (const [id, agent] of live) {
      if (kicked.has(id)) continue;
      kicked.add(id);
      agent.followup(createUserMessage({
        content: [{ type: 'text', text }],
        source: { kind: 'user' },
      }));
    }
  }
  ctx.on('agent/created', ({ agent }) => {
    const agentId = String(agent.id);
    if (!isMine(agentId)) return;
    live.set(agentId, agent);
    announce('loop:agent-created', { agentId });
    kickoff();
  });
  ctx.on('agent/disposed', ({ agent }) => {
    live.delete(String(agent.id));
  });

  // Session events carry no agent, so an assembly cannot tell whose turn it is
  // from the event alone. Ownership of the *session* is the same question one
  // step earlier: a session belongs to whichever agent is running in it.
  const ownedSessions = new Set();
  ctx.on('agent/created', ({ agent }) => {
    if (isMine(String(agent.id)) && agent.session?.id !== undefined) {
      ownedSessions.add(String(agent.session.id));
    }
  });

  // Inbound control face: the embedder drives the loop with ordinary service
  // calls. Commands address the single configured agent by default, or a
  // specific one via `agentId`.
  const resolveAgent = (input) => {
    if (typeof input?.agentId === 'string') return live.get(input.agentId);
    if (live.size === 1) return live.values().next().value;
    return undefined;
  };
  const userMessage = (text) => createUserMessage({
    content: [{ type: 'text', text: String(text) }],
    source: { kind: 'user' },
  });
  sink.service('loop:control', (input) => {
    const agent = resolveAgent(input);
    if (agent === undefined) {
      const known = [...live.keys()];
      throw new Error(`loop:control: no live agent${input?.agentId ? ` "${input.agentId}"` : ''} (live: ${known.join(', ') || '(none)'})`);
    }
    switch (input?.kind) {
      case 'followup':
        agent.followup(userMessage(input.text ?? ''));
        return { accepted: true, agentId: String(agent.id) };
      case 'steer':
        agent.steer(userMessage(input.text ?? ''));
        return { accepted: true, agentId: String(agent.id) };
      case 'cancel':
        agent.cancel({ kind: 'user' });
        return { accepted: true, agentId: String(agent.id) };
      case 'status':
        return { agentId: String(agent.id), status: agent.status };
      default:
        throw new Error(`loop:control: unknown command kind ${JSON.stringify(input?.kind)}`);
    }
  });
}

export default { name, inject, apply };
