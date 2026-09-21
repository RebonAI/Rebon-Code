//! What the platform wants the main thread for while a runtime is serving.
//!
//! On macOS the backend's overlay marshals its AppKit work onto the
//! application's main dispatch queue, and that queue is only drained by a main
//! thread that is running a run loop. A desktop app has one; a process that
//! spends its main thread inside `block_on` does not, and the consequence is
//! not a missing highlight — `Overlay::new` (in this runtime's private
//! `backend` module) marshals **synchronously**, so the first `observe` would
//! hang the worker thread that called it, for good.
//!
//! So a process serving the runtime has to lend its main thread. That is what
//! this is: the serving goes to a worker, the main thread drains the platform's
//! queue until somebody stops it.
//!
//! # Elsewhere
//!
//! Windows has no equivalent — its backend talks to the window from whatever
//! thread it is on — so [`required`] is false there and a caller can simply
//! await. The parker still works on every platform, because an API that only
//! exists on one is an API every caller has to `cfg` around.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How long the main thread goes back to the platform between stop checks.
///
/// Bounds shutdown latency rather than responsiveness: the queue is drained
/// throughout each slice, and this only decides how soon after a stop the park
/// returns.
const SLICE: Duration = Duration::from_millis(200);

/// Whether serving requires this process to give up its main thread.
///
/// A caller that ignores this on macOS does not get a degraded runtime; it gets
/// one that hangs the first time a target is locked.
pub const fn required() -> bool {
    cfg!(target_os = "macos")
}

/// Creates a main-thread park and the handle that ends it.
pub fn pair() -> (Parker, Stopper) {
    let state = Arc::new(State {
        stopped: AtomicBool::new(false),
        waker: Mutex::new(()),
        wake: Condvar::new(),
    });
    (
        Parker {
            state: Arc::clone(&state),
        },
        Stopper { state },
    )
}

struct State {
    stopped: AtomicBool,
    waker: Mutex<()>,
    wake: Condvar,
}

/// Holds the main thread for the platform. Consumed by [`Parker::park`].
pub struct Parker {
    state: Arc<State>,
}

impl Parker {
    /// Blocks the calling thread — which must be the main thread — until the
    /// paired [`Stopper`] fires.
    ///
    /// On macOS this runs the main run loop in slices, which is what drains the
    /// main dispatch queue. Stopping is checked between slices rather than
    /// through `CFRunLoopStop`, because a stop that arrives before the loop is
    /// running would be dropped and the park would never end.
    pub fn park(self) {
        while !self.state.stopped.load(Ordering::Acquire) {
            platform_slice(&self.state);
        }
    }
}

/// Ends a [`Parker`]. Cheap to clone, and safe to fire more than once.
#[derive(Clone)]
pub struct Stopper {
    state: Arc<State>,
}

impl Stopper {
    pub fn stop(&self) {
        self.state.stopped.store(true, Ordering::Release);
        let _guard = self.state.waker.lock().expect("main-thread waker");
        self.state.wake.notify_all();
    }
}

#[cfg(target_os = "macos")]
fn platform_slice(_state: &State) {
    // Runs sources, timers, and the main queue for up to SLICE. Returning early
    // when there is nothing to do is fine: the outer loop re-enters, and the
    // stop flag is checked each time round.
    unsafe {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, SLICE.as_secs_f64(), 0);
    }
}

#[cfg(not(target_os = "macos"))]
fn platform_slice(state: &State) {
    // Nothing to drain, so this is a plain wait — woken by the stopper rather
    // than polled out.
    let guard = state.waker.lock().expect("main-thread waker");
    let _ = state.wake.wait_timeout(guard, SLICE);
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    /// `CFRunLoopMode` is a `CFStringRef`; the mode is only ever passed back.
    static kCFRunLoopDefaultMode: *const std::ffi::c_void;
    /// `CFRunLoopRunResult CFRunLoopRunInMode(CFRunLoopMode, CFTimeInterval, Boolean)`
    fn CFRunLoopRunInMode(
        mode: *const std::ffi::c_void,
        seconds: f64,
        return_after_source_handled: u8,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_park_ends_when_it_is_stopped_from_another_thread() {
        let (parker, stopper) = pair();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            stopper.stop();
        });
        parker.park();
    }

    /// A stop that lands before the park starts must still end it — the whole
    /// reason this checks a flag instead of relying on `CFRunLoopStop`.
    #[test]
    fn a_stop_that_arrives_first_is_not_lost() {
        let (parker, stopper) = pair();
        stopper.stop();
        parker.park();
    }

    #[test]
    fn stopping_twice_is_harmless() {
        let (parker, stopper) = pair();
        stopper.stop();
        stopper.stop();
        parker.park();
    }

    #[test]
    fn only_macos_asks_for_the_main_thread() {
        assert_eq!(required(), cfg!(target_os = "macos"));
    }
}
