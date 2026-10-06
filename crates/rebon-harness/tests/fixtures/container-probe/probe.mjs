// A plugin that reports what its host lets it do: which process it runs in,
// whether it can read or write a path, start a process, or see a variable.
// Loaded once in a container and once on the shared host, the two answers
// are the container's whole contract.
import fs from 'node:fs';
import childProcess from 'node:child_process';
import { defineTool } from '@deepseek-ai/dsh-tools';

export const name = 'container-probe';
export const inject = ['tools'];

function attempt(run) {
  try {
    return { ok: true, value: run() ?? null };
  } catch (error) {
    return { ok: false, code: error.code ?? String(error.message ?? error) };
  }
}

export function apply(ctx, config) {
  ctx.tools.register(defineTool({
    name: config?.tool ?? 'probe',
    description: 'Reports what this plugin host permits.',
    parameters: {
      action: { type: 'string', required: true, description: 'pid | read | write | spawn | env | fetch' },
      path: { type: 'string', description: 'A file path, or a variable name for env.' },
    },
    output: {
      schema: { type: 'string' },
      render: (_args, value) => [{ type: 'text', text: value }],
    },
    async execute(args) {
      let answer;
      switch (args.action) {
        case 'pid': answer = { ok: true, value: process.pid }; break;
        case 'read': answer = attempt(() => fs.readFileSync(args.path, 'utf8').length); break;
        case 'write': answer = attempt(() => fs.writeFileSync(args.path, 'probe')); break;
        case 'spawn': answer = attempt(() => String(childProcess.execSync('node --version'))); break;
        case 'env': answer = { ok: true, value: process.env[args.path] ?? null }; break;
        case 'fetch':
          answer = await fetch(args.path)
            .then((response) => ({ ok: true, value: response.status }))
            .catch((error) => ({
              ok: false,
              code: String(error.cause?.code ?? ''),
              message: `${error.message} ${error.cause?.message ?? ''}`,
            }));
          break;
        default: answer = { ok: false, code: 'UNKNOWN_ACTION' };
      }
      return JSON.stringify(answer);
    },
  }));
}
