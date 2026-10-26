// Test fixture: a Cordis plugin in the realm the upstream bundle provides,
// reaching `ctx.storage` (`dsh-storage` with `dsh-storage-json`) and
// `ctx.skills` (`dsh-skill` with `dsh-skill-filesystem`) the way a dsh
// consumer would, from a tool rebon can call.
import { defineTool } from '@deepseek-ai/dsh-tools';

export const name = 'seam-probe';
export const inject = ['tools', 'storage', 'skills'];

export function apply(ctx) {
  ctx.tools.register(defineTool({
    name: 'seam_probe',
    description: 'Exercises the storage and skills seams.',
    parameters: {
      action: { type: 'string', required: true, description: 'storage or skills' },
      cwd: { type: 'string', description: 'where to look for project skills' },
    },
    output: {
      schema: { type: 'object', additionalProperties: true, properties: {} },
      render: (_args, value) => [{ type: 'text', text: JSON.stringify(value) }],
    },
    execute: async (args) => {
      if (args.action === 'storage') {
        const unit = await ctx.storage.backend.get('json').kv.open({ name: 'probe', version: 1, tables: ['things'], hasGlobal: false });
        try {
          await unit.putRecord('things', 'answer', { n: 42 });
          return { backends: ctx.storage.backend.names(), state: await unit.loadAll() };
        } finally {
          await unit.close();
        }
      }
      if (args.action === 'skills') {
        const skills = await ctx.skills.list({ cwd: args.cwd });
        return { skills: skills.map((skill) => skill.name) };
      }
      throw new Error(`unknown action ${args.action}`);
    },
  }));
}
