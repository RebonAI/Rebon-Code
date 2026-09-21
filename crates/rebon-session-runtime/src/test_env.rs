//! Process-global environment lock shared by tests in this binary.
//!
//! `std::env::set_var` is process-wide, not test-local: a test that swaps
//! `PATH` / `PATHEXT` to exercise command discovery makes *every* test
//! running concurrently in this binary unable to resolve `git`. Both sides
//! take this lock — the tests that mutate the environment and the tests
//! that shell out while it is intact.

use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}
