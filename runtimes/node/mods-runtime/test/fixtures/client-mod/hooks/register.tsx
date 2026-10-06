import type { Register } from 'claude-code';

let label = 'start';
export const heard: unknown[] = [];

export const register: Register = (on) => {
  on('ui.render', { component: 'Pane', requestId: 'game' }, async ($, e) => {
    const { Box, Client, Text } = $.ui.resolve(e);
    if (label === 'none') return <Text>no client</Text>;
    if (label === 'broken') return <Box><Client key="bad" module="./broken.tsx" props={{}} /></Box>;
    if (label === 'loop') return <Box><Client key="loop" module="./loop.tsx" props={{}} /></Box>;
    return (
      <Box flexDirection="column">
        <Text>header</Text>
        <Client key="game" module="./game.tsx" props={{ label }} width={10} height="50%" />
      </Box>
    );
  });
  on('ui.message', async ($, e) => {
    heard.push(e);
    const data = e.data as { want?: string };
    return data.want ? { props: { label: data.want } } : {};
  });
  on('ui.focus', async ($, e, next) => {
    if (e.element === 'locked') return { deny: 'locked stays put' };
    if (e.element === 'alias') return next({ ...e, element: 'real' });
    return next(e);
  });
};

export function setLabel(next: string) {
  label = next;
}
