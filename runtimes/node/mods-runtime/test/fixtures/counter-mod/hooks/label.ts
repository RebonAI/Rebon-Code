export function label(n: number): string {
  return `${n} click${n === 1 ? '' : 's'}`;
}
