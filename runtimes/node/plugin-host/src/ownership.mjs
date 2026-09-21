// Which entry a stray promise belonged to.
//
// Node does not stop this process for an unhandled rejection, and nothing else
// looks either: a plugin could start an async task, have it fail, and leave no
// trace anywhere rebon reads. Making it visible is not the same as attributing
// it — a failed promise does not carry who created it.
//
// Measured on Node 24.19.0, an `AsyncLocalStorage` store survives
// `setTimeout`, `queueMicrotask` and `await`, and the `unhandledRejection`
// handler reads it directly — so wrapping each `plugin/load` is enough, with no
// per-promise `async_hooks` and therefore no standing cost.
//
// **The store follows the async chain, not the clock**, and that distinction is
// the whole reason `loading` exists. A timer started during `apply` that
// rejects ten seconds later still carries its entry, long after that load
// returned. So there are two questions, not one:
//
//   * whose is it — `entry`, always, whenever the chain reaches back;
//   * does it fail the load — `loading`, true only while the load is in flight,
//     because a load that already succeeded cannot be un-failed.
//
// The flag is flipped on the same object rather than by entering a second
// store: a new store would not be seen by promises already rooted in the first.
import { AsyncLocalStorage } from 'node:async_hooks';

const owners = new AsyncLocalStorage();

/// Runs `fn` as `entry`'s work, with the load marked in flight for its
/// duration. Returns whatever `fn` returns; the flag is cleared either way,
/// because a load that threw is over too.
export async function asEntryLoad(entry, fn) {
  const store = { entry, loading: true };
  try {
    const value = await owners.run(store, fn);
    // One turn of the loop before deciding. `unhandledRejection` fires after
    // the microtask queue drains, so a task this load started that failed
    // immediately has not been reported yet at the moment `fn` resolves —
    // checking without this yield would call a broken load a good one.
    await new Promise((resolve) => setImmediate(resolve));
    if (store.rejection !== undefined) {
      throw new Error(`[ENTRY_FAILED] ${entry} left a rejection unhandled while loading: ${store.rejection}`);
    }
    return value;
  } finally {
    store.loading = false;
  }
}

/// The entry whose async chain this code is on, or `undefined` outside one.
///
/// Undefined is normal and not a failure: a rejection can escape the chain
/// through an `EventEmitter`, a native callback or a third-party library, and
/// "could not be determined" is a truthful answer worth keeping distinct from a
/// wrong one.
export function currentOwner() {
  return owners.getStore();
}
