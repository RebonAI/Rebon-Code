import type { Register } from 'claude-code';
import { atom, read, update } from 'claude-code';
import { label } from './label.ts';

const count = atom({ plugin: 'counter', key: 'count' } as const, 0);

export const register: Register = (on, options) => {
  on('session.start', async ($, e, next) => {
    await $.command.register({ name: 'count', description: 'Shows the count', argumentHint: '[reset]' });
    $.ui.status(`${options.prefix ?? ''}0`);
    return next(e);
  });

  on('tool.call', { tool: 'Bash' }, async ($, e, next) => {
    const command = String((e.input as { command?: string }).command ?? '');
    if (command.includes('rm -rf')) return { deny: 'not on my watch' };
    const result = await next({ ...e, input: { ...e.input, command: `${command} # seen` } });
    return { ...result, context: `bash answered ${result.isError ? 'with an error' : 'fine'}` };
  });

  on('prompt.submit', async ($, e, next) => {
    if (e.text === 'drop me') return { drop: 'dropped by counter' };
    return next({ ...e, text: e.text.toUpperCase() });
  });

  on('command.run', { command: 'count' }, async ($) => ({ text: `count is ${await read($, count)}` }));

  on('ui.render', { component: 'Pane', requestId: 'counter' }, async ($, e) => {
    const { Box, Text, Button } = $.ui.resolve(e);
    const n: number = await read($, count);
    return (
      <Box flexDirection="column" borderStyle="round">
        <Text bold>{label(n)}</Text>
        <Button key="more" variant="primary" onPress={() => update($, count, (value) => (value ?? 0) + 1)}>
          more
        </Button>
      </Box>
    );
  });

  on('classic.Stop', async ($, e) => ({ systemMessage: `stopped: ${e.stop_reason}` }));

  on('turn.complete', async ($, e, next) => {
    const answer = await next(e);
    return { text: `${answer.text} (seen by counter)` };
  });
};
