// The `commands` seat: a DeepSeek Harness plugin's slash commands, as rebon's.
//
// `ctx.commands.register({ name, description, input, handler })` is the DSH
// command registry's call, unchanged. Each registration goes on the
// entry's sink as a command of kind `output`: running it asks the plugin
// (`command/invoke`), and what the handler returns is shown where the
// person typed it — a DSH command acts "without sending the command to the
// model", and rebon keeps it that way.
//
// The handler gets the invocation DSH defines: the text after the name, a
// signal, and the agent it ran for — here the session the command was typed
// in, by id, which is all a command handler outside DSH's own engine can be
// told about.
import { Service } from 'cordis';
import { sinkOf } from './registry.mjs';

const NAME = /^[a-z][a-z0-9-]*$/;

let sequence = 0;

/** A DSH `CommandResult` as the text rebon shows. */
function shown(result, name) {
  if (typeof result === 'string') return result;
  if (result && typeof result === 'object') {
    if (result.kind === 'error') {
      throw new Error(typeof result.text === 'string' && result.text ? result.text : `/${name} failed`);
    }
    return typeof result.text === 'string' ? result.text : '';
  }
  return '';
}

export default class RebonCommandsRuntime extends Service {
  constructor(ctx) {
    super(ctx, 'commands');
  }

  /// DSH's `CommandService.register`. Returns the disposer DSH promises; the
  /// registration itself leaves with the entry, which is how rebon removes
  /// every registration a plugin made.
  register(definition) {
    const name = definition?.name;
    if (typeof name !== 'string' || !NAME.test(name)) {
      throw new TypeError(`commands.register: ${JSON.stringify(name)} is not a lowercase command name`);
    }
    if (typeof definition.description !== 'string' || definition.description.length === 0) {
      throw new TypeError(`commands.register: /${name} needs a description`);
    }
    if (typeof definition.handler !== 'function') {
      throw new TypeError(`commands.register: /${name} needs a handler`);
    }
    const sink = sinkOf(this.ctx);
    if (sink === undefined) {
      throw new TypeError(`commands.register: /${name} was registered outside any plugin`);
    }
    const wire = { name, description: definition.description, kind: { type: 'output' } };
    if (typeof definition.input?.hint === 'string' && definition.input.hint.length > 0) {
      wire.hint = definition.input.hint;
    }
    sink.command(wire, async (request, callCtx) => {
      const scope = callCtx?.scopeId ?? callCtx?.scope ?? 'rebon';
      const result = await definition.handler({
        commandId: `rebon-command-${++sequence}`,
        agent: { id: scope, session: { id: scope } },
        rawInput: typeof request?.rest === 'string' ? request.rest : '',
        signal: callCtx?.signal ?? new AbortController().signal,
      });
      return shown(result, name);
    });
    return () => {};
  }
}
