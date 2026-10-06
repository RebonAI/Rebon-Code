type State = { presses: number; ticks: number };

export default function Game(props: { label: string }, surface: any) {
  const { Text } = surface.elements;
  if (surface.state === undefined) {
    surface.setState({ presses: 0, ticks: 0 });
    surface.every(20, () => surface.setState({ ...surface.state, ticks: surface.state.ticks + 1 }));
    surface.onKey((e: { key: string }) => {
      surface.setState({ ...surface.state, presses: surface.state.presses + 1 });
      surface.post({ key: e.key, want: e.key === 'x' ? 'from-post' : undefined });
    });
    return <Text>loading</Text>;
  }
  const s = surface.state as State;
  return <Text>{`${props.label} ${surface.columns}x${surface.rows} presses=${s.presses}`}</Text>;
}
