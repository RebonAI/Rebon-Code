//! A config home this crate's tests can write `remotes.json` into.
//!
//! `REBON_CONFIG_DIR` is process state, and Rust runs a test binary's tests
//! on many threads at once, so a test that points it somewhere has to hold
//! everyone else off while it does. The lock is per test binary because the
//! variable is per process; there is one of these in every crate that needs
//! it, and they are not a shared abstraction waiting to be extracted — a
//! `Mutex` in another crate would guard another process's environment.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Points `REBON_CONFIG_DIR` at a fresh temporary directory, and puts back
/// whatever was there when it drops.
pub struct TestConfigHome {
    // Held for the guard's lifetime, released after the variable is restored.
    _lock: MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl TestConfigHome {
    pub fn new() -> Self {
        // A poisoned lock means some other test panicked while holding it.
        // The environment it left behind is overwritten on the next line
        // either way, so recovering is the useful answer, not a cascade of
        // failures in tests that did nothing wrong.
        let lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let dir = tempfile::Builder::new()
            .prefix("rebon-remote-tests-")
            .tempdir()
            .expect("a temp dir for the test config home");
        let previous = std::env::var_os("REBON_CONFIG_DIR");
        std::env::set_var("REBON_CONFIG_DIR", dir.path());
        Self {
            _lock: lock,
            dir,
            previous,
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for TestConfigHome {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
            None => std::env::remove_var("REBON_CONFIG_DIR"),
        }
    }
}
