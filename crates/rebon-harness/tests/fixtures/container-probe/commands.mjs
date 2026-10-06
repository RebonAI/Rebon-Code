// A DeepSeek Harness-shaped plugin that registers slash commands through
// `ctx.commands`, the way dsh-command-* packages do.
export const name = 'dsh-commands-probe';
export const inject = ['commands'];

export function apply(ctx) {
  ctx.commands.register({
    name: 'greet',
    description: 'Says hello',
    input: { hint: '<name>' },
    handler: ({ rawInput }) => ({ kind: 'success', text: `hello ${rawInput.trim() || 'there'}` }),
  });
  ctx.commands.register({
    name: 'refuse',
    description: 'Always refuses',
    handler: () => ({ kind: 'error', text: 'refused on purpose' }),
  });
}
