//! Process-global environment lock shared by tests in this crate.
//!
//! `std::env::set_var` is process-wide, not test-local: a test that blanks
//! `PATH` to prove a helper binary is unreachable also makes every other
//! test running concurrently unable to spawn `git`, a shell or an MCP
//! server. Both sides take this lock — the tests that mutate the
//! environment hold it exclusively through [`lock_env`], and the tests that
//! shell out while it is intact share it through [`hold_env`], so they keep
//! running in parallel with each other and only ever wait for a mutator.
//!
//! A spawning test that skips [`hold_env`] fails with
//! `No such file or directory (os error 2)` for its shell whenever it
//! overlaps one of the `PATH` tests, which is what the `rebon-tool` battery
//! used to show as a load flake.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

static ENV_LOCK: RwLock<()> = RwLock::new(());

/// Exclusive: for tests that change the environment.
pub(crate) fn lock_env() -> RwLockWriteGuard<'static, ()> {
    ENV_LOCK.write().unwrap_or_else(|err| err.into_inner())
}

/// Shared: for tests that spawn a process and need the environment intact.
pub(crate) fn hold_env() -> RwLockReadGuard<'static, ()> {
    ENV_LOCK.read().unwrap_or_else(|err| err.into_inner())
}
