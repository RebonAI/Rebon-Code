export function Loop(_props: unknown, surface: any) {
  surface.setState((surface.state ?? 0) + 1);
  return surface.elements.Text({ children: `n=${surface.state}` });
}
