// Watchdog probe: a hostile plugin that never yields — its apply
// spins the isolate forever, so the composition can only end by
// termination. The kill path must reclaim it.
export const name = 'spin-plugin';

export function apply() {
  for (;;) {
    // burn
  }
}
